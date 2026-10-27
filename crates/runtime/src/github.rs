//! GitHub App client: installation tokens for the authorizer, and the API calls proposals need.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};

const API: &str = "https://api.github.com";

pub struct GitHubApp {
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    installations: Mutex<HashMap<String, u64>>,
    tokens: Mutex<HashMap<u64, (String, DateTime<Utc>)>>,
    identity: OnceCell<GitIdentity>,
}

/// Who commits made with an app's access are by: its bot user.
#[derive(Clone, Debug)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
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
    /// The commit `head` resolved to.
    pub head_sha: String,
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
            identity: OnceCell::new(),
        })
    }

    /// The app's bot user as a git identity, so commits are attributed to it on GitHub.
    pub async fn identity(&self) -> anyhow::Result<GitIdentity> {
        self.identity
            .get_or_try_init(|| async {
                let app: Value = self
                    .app_get("/app")
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let slug = app["slug"].as_str().context("app without slug")?;
                let login = format!("{slug}[bot]");
                let user: Value = self
                    .http
                    .get(format!("{API}/users/{login}"))
                    .bearer_auth(self.any_token().await?)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let id = user["id"].as_u64().context("bot user without id")?;
                Ok(GitIdentity {
                    email: format!("{id}+{login}@users.noreply.github.com"),
                    name: login,
                })
            })
            .await
            .cloned()
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

    /// A token from any of the app's installations, for reads that aren't tied to one repository.
    pub async fn any_token(&self) -> anyhow::Result<String> {
        let cached = self.installations.lock().await.values().next().copied();
        let installation = match cached {
            Some(id) => id,
            None => {
                let installations: Vec<Value> = self
                    .app_get("/app/installations")
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                installations
                    .first()
                    .and_then(|i| i["id"].as_u64())
                    .context("the GitHub App has no installations")?
            }
        };
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
        let head_sha = self.branch_sha(repo, head).await?;
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
            head_sha,
        })
    }

    /// The commit a branch (or commit) points to.
    pub async fn branch_sha(&self, repo: &str, reference: &str) -> anyhow::Result<String> {
        let commit: Value = self
            .repo_get(
                repo,
                &format!("/commits/{reference}"),
                "application/vnd.github+json",
            )
            .await?
            .error_for_status()?
            .json()
            .await?;
        commit["sha"]
            .as_str()
            .map(str::to_owned)
            .context("commit without sha")
    }

    /// Turns auto-merge off on open pull requests from `branch`, returning their numbers.
    pub async fn disable_auto_merge(&self, repo: &str, branch: &str) -> anyhow::Result<Vec<u64>> {
        let owner = repo.split('/').next().unwrap_or_default();
        let pulls: Vec<Value> = self
            .repo_get(
                repo,
                &format!("/pulls?state=open&head={owner}:{branch}"),
                "application/vnd.github+json",
            )
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut disabled = Vec::new();
        for pull in pulls.iter().filter(|pull| !pull["auto_merge"].is_null()) {
            let token = self.token_for(repo).await?;
            let response: Value = self
                .http
                .post(format!("{API}/graphql"))
                .bearer_auth(token)
                .json(&json!({
                    "query": "mutation($id: ID!) { disablePullRequestAutoMerge(input: {pullRequestId: $id}) { clientMutationId } }",
                    "variables": {"id": pull["node_id"]},
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if let Some(error) = response["errors"].as_array().and_then(|e| e.first()) {
                bail!("{}", error["message"].as_str().unwrap_or("unknown error"));
            }
            disabled.extend(pull["number"].as_u64());
        }
        Ok(disabled)
    }

    async fn repo_post(&self, repo: &str, path: &str, payload: &Value) -> anyhow::Result<Value> {
        let token = self.token_for(repo).await?;
        let response = self
            .http
            .post(format!("{API}/repos/{repo}{path}"))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .json(payload)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "GitHub refused it ({status}): {}",
                body["message"].as_str().unwrap_or("no message")
            );
        }
        Ok(body)
    }

    pub async fn create_pull(&self, repo: &str, payload: &Value) -> anyhow::Result<Value> {
        let body = self.repo_post(repo, "/pulls", payload).await?;
        Ok(json!({"url": body["html_url"], "number": body["number"], "node_id": body["node_id"]}))
    }

    /// Has GitHub merge a pull request once its required checks pass, with a merge method the repository allows.
    pub async fn enable_auto_merge(
        &self,
        repo: &str,
        pull_node_id: &str,
        expected_head: &str,
    ) -> anyhow::Result<()> {
        let info: Value = self
            .repo_get(repo, "", "application/vnd.github+json")
            .await?
            .error_for_status()?
            .json()
            .await?;
        let method = [
            ("allow_squash_merge", "SQUASH"),
            ("allow_merge_commit", "MERGE"),
            ("allow_rebase_merge", "REBASE"),
        ]
        .into_iter()
        .find(|(setting, _)| info[*setting].as_bool() == Some(true))
        .map(|(_, method)| method)
        .context("the repository allows no merge method")?;
        let token = self.token_for(repo).await?;
        let response: Value = self
            .http
            .post(format!("{API}/graphql"))
            .bearer_auth(token)
            .json(&json!({
                "query": "mutation($id: ID!, $method: PullRequestMergeMethod!, $head: GitObjectID!) { \
                          enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method, expectedHeadOid: $head}) \
                          { clientMutationId } }",
                "variables": {"id": pull_node_id, "method": method, "head": expected_head},
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(error) = response["errors"].as_array().and_then(|e| e.first()) {
            bail!("{}", error["message"].as_str().unwrap_or("unknown error"));
        }
        Ok(())
    }

    pub async fn create_issue(&self, repo: &str, payload: &Value) -> anyhow::Result<Value> {
        let body = self.repo_post(repo, "/issues", payload).await?;
        Ok(json!({"url": body["html_url"], "number": body["number"]}))
    }

    /// Creates the labels in `names` that `repo` doesn't have yet.
    pub async fn ensure_labels(&self, repo: &str, names: &[&str]) -> anyhow::Result<()> {
        let mut existing = Vec::new();
        for page in 1.. {
            let labels: Vec<Value> = self
                .repo_get(
                    repo,
                    &format!("/labels?per_page=100&page={page}"),
                    "application/vnd.github+json",
                )
                .await?
                .error_for_status()?
                .json()
                .await?;
            let last = labels.len() < 100;
            existing.extend(
                labels
                    .iter()
                    .filter_map(|l| l["name"].as_str().map(str::to_lowercase)),
            );
            if last {
                break;
            }
        }
        for name in names {
            if !existing.contains(&name.to_lowercase()) {
                self.repo_post(repo, "/labels", &json!({"name": name, "color": "ededed"}))
                    .await
                    .with_context(|| format!("creating the label {name}"))?;
                existing.push(name.to_lowercase());
            }
        }
        Ok(())
    }
}
