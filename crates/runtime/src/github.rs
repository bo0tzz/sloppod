//! GitHub App client: installation tokens for the authorizer, and the API calls proposals need.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

const API: &str = "https://api.github.com";

/// Lookups the authorizer makes while deciding a request, which Envoy gives 10 s in all.
const AUTHZ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a repository's installation is trusted: reinstalling the app changes it.
const INSTALLATION_TTL: Duration = Duration::from_secs(600);
/// How long "not installed" is trusted, so reads of public repositories don't ask GitHub every time.
const NOT_INSTALLED_TTL: Duration = Duration::from_secs(60);

/// One cached token, minted by whichever caller gets the slot first; the others wait for it.
type TokenSlot = Arc<tokio::sync::Mutex<Option<AccessToken>>>;

pub struct GitHubApp {
    api: String,
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    /// Repository to installation, or `None` for not installed, with when it was looked up.
    installations: Mutex<HashMap<String, (Option<u64>, Instant)>>,
    tokens: Mutex<HashMap<u64, TokenSlot>>,
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

impl AccessToken {
    fn fresh(&self) -> bool {
        self.expires_at - Utc::now() > chrono::Duration::minutes(5)
    }
}

fn installation_cached(entry: (Option<u64>, Instant), now: Instant) -> Option<Option<u64>> {
    let (id, at) = entry;
    let ttl = if id.is_some() {
        INSTALLATION_TTL
    } else {
        NOT_INSTALLED_TTL
    };
    (now.duration_since(at) < ttl).then_some(id)
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
        Self::with_api(API, app_id, private_key_pem)
    }

    fn with_api(api: &str, app_id: String, private_key_pem: &[u8]) -> anyhow::Result<Self> {
        Ok(Self {
            api: api.to_owned(),
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
                    .get(self.url(&format!("/users/{login}")))
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

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api)
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
            .get(self.url(path))
            .bearer_auth(self.jwt()?)
            .header("accept", "application/vnd.github+json")
            .timeout(AUTHZ_TIMEOUT)
            .send()
            .await?;
        Ok(response)
    }

    fn remember_installation(&self, repo: &str, id: Option<u64>) {
        self.installations
            .lock()
            .unwrap()
            .insert(repo.to_owned(), (id, Instant::now()));
    }

    async fn installation_for(&self, repo: &str) -> anyhow::Result<u64> {
        let cached = self.installations.lock().unwrap().get(repo).copied();
        let id = match cached.and_then(|entry| installation_cached(entry, Instant::now())) {
            Some(id) => id,
            None => {
                let response = self
                    .http
                    .get(self.url(&format!("/repos/{repo}/installation")))
                    .bearer_auth(self.jwt()?)
                    .header("accept", "application/vnd.github+json")
                    .timeout(AUTHZ_TIMEOUT)
                    .send()
                    .await?;
                let id = match response.status() {
                    reqwest::StatusCode::NOT_FOUND => None,
                    status if !status.is_success() => {
                        bail!(
                            "looking up the GitHub App's installation on {repo} failed ({status})"
                        )
                    }
                    _ => Some(
                        response.json::<Value>().await?["id"]
                            .as_u64()
                            .context("installation without id")?,
                    ),
                };
                self.remember_installation(repo, id);
                id
            }
        };
        id.with_context(|| format!("the GitHub App is not installed on {repo}"))
    }

    /// Drops everything cached about an installation that no longer exists, e.g. after the app was reinstalled.
    fn forget_installation(&self, installation: u64) {
        self.installations
            .lock()
            .unwrap()
            .retain(|_, (id, _)| *id != Some(installation));
        self.tokens.lock().unwrap().remove(&installation);
    }

    /// A token for `installation`, or `None` if the installation is gone.
    async fn installation_token(&self, installation: u64) -> anyhow::Result<Option<String>> {
        let slot = self
            .tokens
            .lock()
            .unwrap()
            .entry(installation)
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if let Some(token) = slot.as_ref().filter(|t| t.fresh()) {
            return Ok(Some(token.token.clone()));
        }
        let response = self
            .http
            .post(self.url(&format!("/app/installations/{installation}/access_tokens")))
            .bearer_auth(self.jwt()?)
            .header("accept", "application/vnd.github+json")
            .timeout(AUTHZ_TIMEOUT)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            drop(slot);
            self.forget_installation(installation);
            return Ok(None);
        }
        let token: AccessToken = response.error_for_status()?.json().await?;
        let value = token.token.clone();
        *slot = Some(token);
        Ok(Some(value))
    }

    /// An installation token that can act on `repo`.
    pub async fn token_for(&self, repo: &str) -> anyhow::Result<String> {
        // A second try looks the installation up afresh, in case the cached one was removed.
        for _ in 0..2 {
            let installation = self.installation_for(repo).await?;
            if let Some(token) = self.installation_token(installation).await? {
                return Ok(token);
            }
        }
        bail!("the GitHub App's installation on {repo} keeps disappearing")
    }

    /// A token from any of the app's installations, for reads that aren't tied to one repository.
    pub async fn any_token(&self) -> anyhow::Result<String> {
        for _ in 0..2 {
            let cached = self
                .installations
                .lock()
                .unwrap()
                .values()
                .find_map(|(id, _)| *id);
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
            if let Some(token) = self.installation_token(installation).await? {
                return Ok(token);
            }
        }
        bail!("the GitHub App's installations keep disappearing")
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
            let Some(token) = self.installation_token(id).await? else {
                continue;
            };
            let page: Value = self
                .http
                .get(self.url("/installation/repositories?per_page=100"))
                .bearer_auth(token)
                .header("accept", "application/vnd.github+json")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for repo in page["repositories"].as_array().into_iter().flatten() {
                if let Some(name) = repo["full_name"].as_str() {
                    self.remember_installation(name, Some(id));
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
            .get(self.url(&format!("/repos/{repo}{path}")))
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
                .post(self.url("/graphql"))
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
            .post(self.url(&format!("/repos/{repo}{path}")))
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
            .post(self.url("/graphql"))
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    use super::*;

    /// Installation of `o/other`, which never changes.
    const OTHER: u64 = 10;

    /// A fake GitHub: `o/r` is on the current installation, `o/other` on [`OTHER`], other repositories have none.
    #[derive(Default)]
    struct Fake {
        installation: AtomicU64,
        lookups: AtomicUsize,
        mints: AtomicUsize,
        /// Holds each mint until this many are in flight at once.
        rendezvous: Option<tokio::sync::Barrier>,
    }

    async fn installation(
        State(fake): State<Arc<Fake>>,
        Path((owner, name)): Path<(String, String)>,
    ) -> Result<Json<Value>, StatusCode> {
        fake.lookups.fetch_add(1, Ordering::SeqCst);
        let id = match (owner.as_str(), name.as_str()) {
            ("o", "r") => fake.installation.load(Ordering::SeqCst),
            ("o", "other") => OTHER,
            _ => return Err(StatusCode::NOT_FOUND),
        };
        Ok(Json(json!({"id": id})))
    }

    async fn mint(
        State(fake): State<Arc<Fake>>,
        Path(id): Path<u64>,
    ) -> Result<Json<Value>, StatusCode> {
        fake.mints.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(rendezvous) = &fake.rendezvous {
            tokio::time::timeout(Duration::from_secs(2), rendezvous.wait())
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
        if id != fake.installation.load(Ordering::SeqCst) && id != OTHER {
            return Err(StatusCode::NOT_FOUND);
        }
        let expires = Utc::now() + chrono::Duration::hours(1);
        Ok(Json(
            json!({"token": format!("token-{id}"), "expires_at": expires}),
        ))
    }

    async fn fake_github() -> (Arc<Fake>, GitHubApp) {
        serve(Fake::default()).await
    }

    async fn serve(fake: Fake) -> (Arc<Fake>, GitHubApp) {
        let fake = Arc::new(fake);
        fake.installation.store(1, Ordering::SeqCst);
        let router = Router::new()
            .route("/repos/{owner}/{name}/installation", get(installation))
            .route("/app/installations/{id}/access_tokens", post(mint))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256).unwrap();
        let app = GitHubApp::with_api(&api, "1".into(), key.serialize_pem().as_bytes()).unwrap();
        (fake, app)
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_mint() {
        let (fake, app) = fake_github().await;
        let tokens = futures::future::join_all((0..10).map(|_| app.token_for("o/r"))).await;
        assert!(tokens.iter().all(|t| t.as_deref().ok() == Some("token-1")));
        assert_eq!(fake.mints.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn installations_mint_independently() {
        let (_, app) = serve(Fake {
            rendezvous: Some(tokio::sync::Barrier::new(2)),
            ..Fake::default()
        })
        .await;
        let (r, other) = tokio::join!(app.token_for("o/r"), app.token_for("o/other"));
        assert_eq!(r.unwrap(), "token-1");
        assert_eq!(other.unwrap(), format!("token-{OTHER}"));
    }

    #[tokio::test]
    async fn remembers_repositories_without_an_installation() {
        let (fake, app) = fake_github().await;
        assert!(app.token_for("someone/public").await.is_err());
        assert!(app.token_for("someone/public").await.is_err());
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn follows_a_reinstall() {
        let (fake, app) = fake_github().await;
        assert_eq!(app.token_for("o/r").await.unwrap(), "token-1");
        fake.installation.store(2, Ordering::SeqCst);
        app.tokens.lock().unwrap().clear();
        assert_eq!(app.token_for("o/r").await.unwrap(), "token-2");
    }
}
