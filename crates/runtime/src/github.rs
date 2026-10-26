//! GitHub App client: installation tokens for the authorizer, and the API calls proposals need.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

const API: &str = "https://api.github.com";

pub struct GitHubApp {
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    installations: Mutex<HashMap<String, u64>>,
    tokens: Mutex<HashMap<u64, (String, DateTime<Utc>)>>,
}

#[derive(Deserialize)]
struct AccessToken {
    token: String,
    expires_at: DateTime<Utc>,
}

pub struct Comparison {
    pub diff: String,
    pub commits: Vec<(String, String)>,
    pub ahead_by: u64,
}

impl GitHubApp {
    pub fn new(app_id: String, private_key_pem: &[u8]) -> anyhow::Result<Self> {
        Ok(Self {
            app_id,
            key: EncodingKey::from_rsa_pem(private_key_pem)
                .context("parsing the GitHub App private key")?,
            http: reqwest::Client::builder()
                .user_agent("sloppod")
                .timeout(Duration::from_secs(60))
                .build()?,
            installations: Mutex::default(),
            tokens: Mutex::default(),
        })
    }

    fn jwt(&self) -> anyhow::Result<String> {
        let now = Utc::now().timestamp();
        let claims = json!({"iat": now - 60, "exp": now + 540, "iss": self.app_id});
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.key,
        )?)
    }

    async fn app_get(&self, path: &str) -> anyhow::Result<reqwest::Response> {
        let response = self
            .http
            .get(format!("{API}{path}"))
            .bearer_auth(self.jwt()?)
            .header("accept", "application/vnd.github+json")
            .send()
            .await?;
        Ok(response)
    }

    async fn installation_for(&self, repo: &str) -> anyhow::Result<u64> {
        if let Some(id) = self.installations.lock().await.get(repo) {
            return Ok(*id);
        }
        let response = self.app_get(&format!("/repos/{repo}/installation")).await?;
        if !response.status().is_success() {
            bail!(
                "the GitHub App is not installed on {repo} ({})",
                response.status()
            );
        }
        let id = response.json::<Value>().await?["id"]
            .as_u64()
            .context("installation without id")?;
        self.installations.lock().await.insert(repo.to_owned(), id);
        Ok(id)
    }

    async fn installation_token(&self, installation: u64) -> anyhow::Result<String> {
        let mut tokens = self.tokens.lock().await;
        if let Some((token, expires)) = tokens.get(&installation)
            && *expires - Utc::now() > chrono::Duration::minutes(5)
        {
            return Ok(token.clone());
        }
        let response = self
            .http
            .post(format!(
                "{API}/app/installations/{installation}/access_tokens"
            ))
            .bearer_auth(self.jwt()?)
            .header("accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;
        let token: AccessToken = response.json().await?;
        tokens.insert(installation, (token.token.clone(), token.expires_at));
        Ok(token.token)
    }

    /// An installation token that can act on `repo`.
    pub async fn token_for(&self, repo: &str) -> anyhow::Result<String> {
        let installation = self.installation_for(repo).await?;
        self.installation_token(installation).await
    }

    /// Repositories the app is installed on.
    pub async fn repos(&self) -> anyhow::Result<Vec<String>> {
        let installations: Vec<Value> = self
            .app_get("/app/installations")
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut repos = Vec::new();
        for installation in installations {
            let Some(id) = installation["id"].as_u64() else {
                continue;
            };
            let token = self.installation_token(id).await?;
            let page: Value = self
                .http
                .get(format!("{API}/installation/repositories?per_page=100"))
                .bearer_auth(token)
                .header("accept", "application/vnd.github+json")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for repo in page["repositories"].as_array().into_iter().flatten() {
                if let Some(name) = repo["full_name"].as_str() {
                    self.installations.lock().await.insert(name.to_owned(), id);
                    repos.push(name.to_owned());
                }
            }
        }
        repos.sort();
        Ok(repos)
    }

    async fn repo_get(
        &self,
        repo: &str,
        path: &str,
        accept: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let token = self.token_for(repo).await?;
        Ok(self
            .http
            .get(format!("{API}/repos/{repo}{path}"))
            .bearer_auth(token)
            .header("accept", accept)
            .send()
            .await?)
    }

    pub async fn default_branch(&self, repo: &str) -> anyhow::Result<String> {
        let info: Value = self
            .repo_get(repo, "", "application/vnd.github+json")
            .await?
            .error_for_status()?
            .json()
            .await?;
        info["default_branch"]
            .as_str()
            .map(str::to_owned)
            .context("repository without default branch")
    }

    pub async fn compare(&self, repo: &str, base: &str, head: &str) -> anyhow::Result<Comparison> {
        let range = format!("/compare/{base}...{head}");
        let response = self
            .repo_get(repo, &range, "application/vnd.github+json")
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("branch {head} not found on {repo}; push it first");
        }
        let info: Value = response.error_for_status()?.json().await?;
        let commits = info["commits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                (
                    c["sha"].as_str().unwrap_or_default().to_owned(),
                    c["commit"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect();
        let diff = self
            .repo_get(repo, &range, "application/vnd.github.diff")
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(Comparison {
            diff,
            commits,
            ahead_by: info["ahead_by"].as_u64().unwrap_or(0),
        })
    }

    pub async fn create_pull(&self, repo: &str, payload: &Value) -> anyhow::Result<Value> {
        let token = self.token_for(repo).await?;
        let response = self
            .http
            .post(format!("{API}/repos/{repo}/pulls"))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .json(payload)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "GitHub refused the pull request ({status}): {}",
                body["message"].as_str().unwrap_or("no message")
            );
        }
        Ok(json!({"url": body["html_url"], "number": body["number"]}))
    }
}
