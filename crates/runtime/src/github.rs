//! GitHub App client: installation tokens for the authorizer, and the API calls proposals need.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

pub const API: &str = "https://api.github.com";

/// Lookups the authorizer makes while deciding a request, which Envoy gives 10 s in all.
const AUTHZ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a repository's installation is trusted: reinstalling the app changes it.
const INSTALLATION_TTL: Duration = Duration::from_secs(600);
/// How long "not installed" is trusted, so reads of public repositories don't ask GitHub every time.
const NOT_INSTALLED_TTL: Duration = Duration::from_secs(60);

/// What a session's read token may have, as far as the installation has them. A fixed list rather than
/// every installation permission at `read`, because some (`workflows`) have no read level.
const READ_PERMISSIONS: &[&str] = &[
    "actions",
    "checks",
    "contents",
    "issues",
    "metadata",
    "pull_requests",
    "statuses",
];

/// When the authorizer let pushes to branches through. A push it admitted may still be landing, so work that needs
/// branches to hold still checks here as well as their heads. The runtime is one process (ADR 14), so this is too.
#[derive(Default)]
pub struct PushLog {
    admitted: Mutex<HashMap<(String, String), Instant>>,
}

impl PushLog {
    pub fn admit(&self, repo: &str, branch: &str) {
        self.admitted
            .lock()
            .unwrap()
            .insert((repo.to_lowercase(), branch.to_owned()), Instant::now());
    }

    /// The first of `branches` a push was let through to since `settle` before `since`.
    pub fn pushed_since<'a>(
        &self,
        repo: &str,
        branches: &[&'a str],
        since: Instant,
        settle: Duration,
    ) -> Option<&'a str> {
        let admitted = self.admitted.lock().unwrap();
        branches.iter().copied().find(|branch| {
            admitted
                .get(&(repo.to_lowercase(), (*branch).to_owned()))
                .is_some_and(|at| *at + settle >= since)
        })
    }
}

/// Items per page when reading GitHub's lists.
const PAGE: usize = 100;

/// Every item of a paged GitHub list: `fetch` gets page 1, 2, … until one comes back short.
async fn all_pages<F, Fut>(mut fetch: F) -> anyhow::Result<Vec<Value>>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Vec<Value>>>,
{
    let mut all = Vec::new();
    for page in 1.. {
        let items = fetch(page).await?;
        let last = items.len() < PAGE;
        all.extend(items);
        if last {
            break;
        }
    }
    Ok(all)
}

/// How long a push the authorizer let through may take to land, for work that needs branches to hold still.
const PUSH_SETTLE: Duration = Duration::from_secs(10 * 60);

/// What [`GitHubApp::check_merging_stack`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum StackWatch {
    /// Merged, closed, or no longer set to merge: nothing left to watch.
    Done,
    /// Every layer is still at its proposed commit.
    Holding,
    /// This layer moved, and the stack's auto-merge is now off.
    Moved(u64),
}

/// A linked stack to merge: its number, its pull requests and each layer's branch and proposed commit, bottom to
/// top, and the top pull request's node id.
#[derive(Clone, Copy)]
pub struct StackMerge<'a> {
    pub stack: u64,
    pub pulls: &'a [u64],
    pub top_node_id: &'a str,
    pub layers: &'a [(&'a str, &'a str)],
}

/// A query string, `?k=v&…`, with every value encoded: a branch name with `#` or `&` in it must not end the value.
fn query(pairs: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", utf8_percent_encode(value, NON_ALPHANUMERIC)))
        .collect();
    format!("?{}", pairs.join("&"))
}

/// A branch as GitHub's pull request filters name it, `owner:branch`.
fn head_ref(repo: &str, branch: &str) -> String {
    let owner = repo.split('/').next().unwrap_or_default();
    format!("{owner}:{branch}")
}

/// A branch name as a URL path segment: its `/`s stay, since GitHub reads them as part of the name there.
fn branch_in_path(branch: &str) -> String {
    const KEEP: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'.')
        .remove(b'_');
    utf8_percent_encode(branch, KEEP).to_string()
}

/// One cached token, minted by whichever caller gets the slot first; the others wait for it.
type TokenSlot = Arc<tokio::sync::Mutex<Option<AccessToken>>>;

/// What a token is minted for. `None` is everything the installation has.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TokenScope {
    installation: u64,
    /// Repository names without the owner, lowercase and sorted.
    repositories: Option<Vec<String>>,
    permissions: Option<BTreeMap<String, String>>,
}

impl TokenScope {
    fn full(installation: u64) -> Self {
        TokenScope {
            installation,
            repositories: None,
            permissions: None,
        }
    }

    fn body(&self) -> Value {
        let mut body = json!({});
        if let Some(repositories) = &self.repositories {
            body["repositories"] = json!(repositories);
        }
        if let Some(permissions) = &self.permissions {
            body["permissions"] = json!(permissions);
        }
        body
    }
}

/// Repository names for a token's `repositories`, which are relative to the installation's account.
fn repository_names<'a>(repos: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut names: Vec<String> = repos
        .into_iter()
        .map(|r| r.rsplit('/').next().unwrap_or(r).to_lowercase())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn read_permissions(installation: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    READ_PERMISSIONS
        .iter()
        .filter(|p| installation.contains_key(**p))
        .map(|p| (p.to_string(), "read".to_owned()))
        .collect()
}

/// A push token's permissions: `contents: write`, and `workflows: write` too where the installation has it, so a push
/// may change workflow files. Asking for a permission the installation lacks gets the whole token refused.
fn push_permissions(installation: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut permissions = BTreeMap::from([
        ("contents".to_owned(), "write".to_owned()),
        ("metadata".to_owned(), "read".to_owned()),
    ]);
    if installation
        .get("workflows")
        .is_some_and(|level| level == "write")
    {
        permissions.insert("workflows".to_owned(), "write".to_owned());
    }
    permissions
}

/// The span to run a user-token client's calls in, so each logged call names the session and credential it's for.
/// A credential that's only chosen inside the span (`None`) is recorded there once it is (`record_credential`).
pub fn acting_span(session: &str, credential: Option<&str>) -> tracing::Span {
    let span =
        tracing::info_span!("acting", session = %session, credential = tracing::field::Empty);
    if let Some(credential) = credential {
        span.record("credential", credential);
    }
    span
}

/// Records the credential chosen for the calls that follow in the current `acting_span`.
pub fn record_credential(credential: Option<&str>) {
    tracing::Span::current().record("credential", credential.unwrap_or("app"));
}

/// A GitHub client acting with one identity (ADR 29): the owner's GitHub App, through its installations' tokens, or
/// one of the owner's personal access tokens, used as it is.
pub struct GitHubApp {
    api: String,
    app_id: String,
    /// The App's key; none when acting with a user's token.
    key: Option<EncodingKey>,
    /// The user's token and login, when acting as them instead of the App.
    user: Option<(String, String)>,
    http: reqwest::Client,
    /// Repository (lowercase) to installation, or `None` for not installed, with when it was looked up.
    installations: Mutex<HashMap<String, (Option<u64>, Instant)>>,
    /// What each installation was granted, as `permission: level`.
    permissions: Mutex<HashMap<u64, BTreeMap<String, String>>>,
    tokens: Mutex<HashMap<TokenScope, TokenSlot>>,
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
            key: Some(
                EncodingKey::from_rsa_pem(private_key_pem)
                    .context("parsing the GitHub App private key")?,
            ),
            user: None,
            http: reqwest::Client::builder()
                .user_agent("sloppod")
                .timeout(Duration::from_secs(60))
                .build()?,
            installations: Mutex::default(),
            permissions: Mutex::default(),
            tokens: Mutex::default(),
            identity: OnceCell::new(),
        })
    }

    /// A client acting as a user, with their fine-grained token: every repository call carries the token as it is.
    pub fn for_user(token: String, login: String) -> anyhow::Result<Self> {
        Self::for_user_with_api(API, token, login)
    }

    fn for_user_with_api(api: &str, token: String, login: String) -> anyhow::Result<Self> {
        Ok(Self {
            api: api.to_owned(),
            app_id: String::new(),
            key: None,
            user: Some((token, login)),
            http: reqwest::Client::builder()
                .user_agent("sloppod")
                .timeout(Duration::from_secs(60))
                .build()?,
            installations: Mutex::default(),
            permissions: Mutex::default(),
            tokens: Mutex::default(),
            identity: OnceCell::new(),
        })
    }

    /// Who this client acts as: the user's login, or none for the App.
    pub fn user_login(&self) -> Option<&str> {
        self.user.as_ref().map(|(_, login)| login.as_str())
    }

    /// Logs a call the runtime makes with a user's token (ADR 28's audit). Callers run it inside a span naming the
    /// session and credential it's made for (`acting_span`).
    fn audit(&self, method: &str, path: &str) {
        if let Some(login) = self.user_login() {
            tracing::info!(as_user = login, %method, %path, "runtime call with the owner's GitHub token");
        }
    }

    /// The user's token, when acting as a user: it's used for every repository, whatever the scope asked for.
    fn user_token(&self) -> Option<String> {
        self.user.as_ref().map(|(token, _)| token.clone())
    }

    /// The app's bot user as a git identity, so commits are attributed to it on GitHub; the user, for a user's token.
    pub async fn identity(&self) -> anyhow::Result<GitIdentity> {
        if let Some(login) = self.user_login() {
            return Ok(GitIdentity {
                name: login.to_owned(),
                email: format!("{login}@users.noreply.github.com"),
            });
        }
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
            self.key
                .as_ref()
                .context("acting with a user's token, not as the GitHub App")?,
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

    fn remember_installation(&self, repo: &str, installation: &Value) {
        let id = installation["id"].as_u64();
        if let (Some(id), Some(permissions)) = (id, installation["permissions"].as_object()) {
            let permissions = permissions
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect();
            self.permissions.lock().unwrap().insert(id, permissions);
        }
        self.installations
            .lock()
            .unwrap()
            .insert(repo.to_lowercase(), (id, Instant::now()));
    }

    async fn installation_for(&self, repo: &str) -> anyhow::Result<u64> {
        let cached = self
            .installations
            .lock()
            .unwrap()
            .get(&repo.to_lowercase())
            .copied();
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
                let installation = match response.status() {
                    reqwest::StatusCode::NOT_FOUND => Value::Null,
                    status if !status.is_success() => {
                        bail!(
                            "looking up the GitHub App's installation on {repo} failed ({status})"
                        )
                    }
                    _ => response.json::<Value>().await?,
                };
                self.remember_installation(repo, &installation);
                installation["id"].as_u64()
            }
        };
        id.with_context(|| format!("the GitHub App is not installed on {repo}"))
    }

    /// What `installation` was granted, or `None` if it's gone.
    async fn installation_permissions(
        &self,
        installation: u64,
    ) -> anyhow::Result<Option<BTreeMap<String, String>>> {
        if let Some(permissions) = self.permissions.lock().unwrap().get(&installation) {
            return Ok(Some(permissions.clone()));
        }
        let response = self
            .app_get(&format!("/app/installations/{installation}"))
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            self.forget_installation(installation);
            return Ok(None);
        }
        let info: Value = response.error_for_status()?.json().await?;
        let permissions: BTreeMap<String, String> = info["permissions"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect();
        self.permissions
            .lock()
            .unwrap()
            .insert(installation, permissions.clone());
        Ok(Some(permissions))
    }

    /// Drops everything cached about an installation that no longer exists, e.g. after the app was reinstalled.
    fn forget_installation(&self, installation: u64) {
        self.installations
            .lock()
            .unwrap()
            .retain(|_, (id, _)| *id != Some(installation));
        self.permissions.lock().unwrap().remove(&installation);
        self.tokens
            .lock()
            .unwrap()
            .retain(|scope, _| scope.installation != installation);
    }

    /// A token for `scope`, or `None` if its installation is gone.
    async fn mint(&self, scope: TokenScope) -> anyhow::Result<Option<String>> {
        let slot = self
            .tokens
            .lock()
            .unwrap()
            .entry(scope.clone())
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if let Some(token) = slot.as_ref().filter(|t| t.fresh()) {
            return Ok(Some(token.token.clone()));
        }
        let installation = scope.installation;
        let response = self
            .http
            .post(self.url(&format!("/app/installations/{installation}/access_tokens")))
            .bearer_auth(self.jwt()?)
            .header("accept", "application/vnd.github+json")
            .timeout(AUTHZ_TIMEOUT)
            .json(&scope.body())
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => {
                drop(slot);
                self.forget_installation(installation);
                return Ok(None);
            }
            status if !status.is_success() => {
                let body: Value = response.json().await.unwrap_or(Value::Null);
                bail!(
                    "GitHub refused a token for installation {installation} ({status}): {}",
                    body["message"].as_str().unwrap_or("no message")
                );
            }
            _ => {}
        }
        let token: AccessToken = response.json().await?;
        let value = token.token.clone();
        *slot = Some(token);
        Ok(Some(value))
    }

    /// Mints a token for the scope `scope` works out, or `None` if its installation is gone. A second try works the
    /// scope out afresh, since a gone installation is forgotten.
    async fn mint_scoped<F, Fut>(&self, scope: F) -> anyhow::Result<Option<String>>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = anyhow::Result<Option<TokenScope>>>,
    {
        for _ in 0..2 {
            let Some(scope) = scope().await? else {
                continue;
            };
            if let Some(token) = self.mint(scope).await? {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    /// A token with everything the installation on `repo` has. Only for the runtime's own calls, never sessions.
    pub async fn token_for(&self, repo: &str) -> anyhow::Result<String> {
        if let Some(token) = self.user_token() {
            return Ok(token);
        }
        self.mint_scoped(|| async move {
            Ok(Some(TokenScope::full(self.installation_for(repo).await?)))
        })
        .await?
        .with_context(|| format!("the GitHub App's installation on {repo} keeps disappearing"))
    }

    /// A session's read-only token, through the installation on `repo`: it covers the repositories in `granted`
    /// that share that installation (`repo` among them), with [`READ_PERMISSIONS`].
    pub async fn read_token(&self, repo: &str, granted: &[&str]) -> anyhow::Result<String> {
        if let Some(token) = self.user_token() {
            return Ok(token);
        }
        self.mint_scoped(|| async move {
            let installation = self.installation_for(repo).await?;
            let mut covered = vec![repo];
            for other in granted {
                if !sloppod_contracts::same_repo(other, repo)
                    && self.installation_for(other).await.ok() == Some(installation)
                {
                    covered.push(other);
                }
            }
            let Some(permissions) = self.installation_permissions(installation).await? else {
                return Ok(None);
            };
            Ok(Some(TokenScope {
                installation,
                repositories: Some(repository_names(covered)),
                permissions: Some(read_permissions(&permissions)),
            }))
        })
        .await?
        .with_context(|| format!("the GitHub App's installation on {repo} keeps disappearing"))
    }

    /// A session's token for pushing to `repo`: that repository only, with [`push_permissions`].
    pub async fn push_token(&self, repo: &str) -> anyhow::Result<String> {
        if let Some(token) = self.user_token() {
            return Ok(token);
        }
        self.mint_scoped(|| async move {
            let installation = self.installation_for(repo).await?;
            let Some(permissions) = self.installation_permissions(installation).await? else {
                return Ok(None);
            };
            Ok(Some(TokenScope {
                installation,
                repositories: Some(repository_names([repo])),
                permissions: Some(push_permissions(&permissions)),
            }))
        })
        .await?
        .with_context(|| format!("the GitHub App's installation on {repo} keeps disappearing"))
    }

    /// A token from any of the app's installations, for the runtime's reads that aren't tied to one repository.
    pub async fn any_token(&self) -> anyhow::Result<String> {
        if let Some(token) = self.user_token() {
            return Ok(token);
        }
        self.mint_scoped(|| async move {
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
            Ok(Some(TokenScope::full(installation)))
        })
        .await?
        .context("the GitHub App's installations keep disappearing")
    }

    /// Repositories the app is installed on.
    pub async fn repos(&self) -> anyhow::Result<Vec<String>> {
        let installations = all_pages(|page| async move {
            Ok(self
                .app_get(&format!("/app/installations?per_page={PAGE}&page={page}"))
                .await?
                .error_for_status()?
                .json()
                .await?)
        })
        .await?;
        let mut repos = Vec::new();
        for installation in installations {
            let Some(id) = installation["id"].as_u64() else {
                continue;
            };
            let Some(token) = self.mint(TokenScope::full(id)).await? else {
                continue;
            };
            let token = &token;
            let accessible = all_pages(|page| async move {
                let listed: Value = self
                    .http
                    .get(self.url(&format!(
                        "/installation/repositories?per_page={PAGE}&page={page}"
                    )))
                    .bearer_auth(token)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(listed["repositories"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default())
            })
            .await?;
            for repo in &accessible {
                if let Some(name) = repo["full_name"].as_str() {
                    self.remember_installation(name, &installation);
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
        self.audit("GET", &format!("/repos/{repo}{path}"));
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
        self.compare_if_found(repo, base, head)
            .await?
            .ok_or_else(|| anyhow::anyhow!("branch {head} not found on {repo}; push it first"))
    }

    /// Compares `head` with `base`; `None` when either isn't on GitHub, e.g. a branch that was never pushed.
    pub async fn compare_if_found(
        &self,
        repo: &str,
        base: &str,
        head: &str,
    ) -> anyhow::Result<Option<Comparison>> {
        let range = format!(
            "/compare/{}...{}",
            branch_in_path(base),
            branch_in_path(head)
        );
        let response = self
            .repo_get(repo, &range, "application/vnd.github+json")
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
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
        Ok(Some(Comparison {
            diff,
            commits,
            ahead_by: info["ahead_by"].as_u64().unwrap_or(0),
            head_sha,
        }))
    }

    /// The commit a branch (or commit) points to.
    pub async fn branch_sha(&self, repo: &str, reference: &str) -> anyhow::Result<String> {
        let commit: Value = self
            .repo_json(repo, &format!("/commits/{}", branch_in_path(reference)))
            .await?;
        commit["sha"]
            .as_str()
            .map(str::to_owned)
            .context("commit without sha")
    }

    /// Turns auto-merge off on open pull requests from `branch`, returning their numbers.
    pub async fn disable_auto_merge(&self, repo: &str, branch: &str) -> anyhow::Result<Vec<u64>> {
        let headed: Vec<Value> = self
            .repo_json(
                repo,
                &format!(
                    "/pulls{}",
                    query(&[("state", "open"), ("head", &head_ref(repo, branch))])
                ),
            )
            .await?;
        // A stack merges through auto-merge on its top pull request, so a push to any layer turns that off too.
        let mut numbers: Vec<u64> = Vec::new();
        for pull in headed.iter().filter_map(|p| p["number"].as_u64()) {
            numbers.push(pull);
            if let Some(stack) = self.stack_of(repo, pull).await? {
                numbers.extend(self.stack_members(repo, stack).await?);
            }
        }
        numbers.sort_unstable();
        numbers.dedup();
        let mut disabled = Vec::new();
        for number in numbers {
            let pull: Value = self.repo_json(repo, &format!("/pulls/{number}")).await?;
            if pull["state"] != "open" || pull["auto_merge"].is_null() {
                continue;
            }
            self.disable_pull_auto_merge_in(repo, pull["node_id"].as_str().unwrap_or_default())
                .await?;
            disabled.push(number);
        }
        Ok(disabled)
    }

    pub async fn disable_pull_auto_merge_in(
        &self,
        repo: &str,
        node_id: &str,
    ) -> anyhow::Result<()> {
        self.graphql(
            repo,
            "mutation($id: ID!) { disablePullRequestAutoMerge(input: {pullRequestId: $id}) { clientMutationId } }",
            json!({"id": node_id}),
        )
        .await
        .map(|_| ())
    }

    /// Runs a GraphQL query or mutation with the repository's token; GraphQL errors come back as errors.
    pub(crate) async fn graphql(
        &self,
        repo: &str,
        query: &str,
        variables: Value,
    ) -> anyhow::Result<Value> {
        let token = self.token_for(repo).await?;
        self.audit("POST", &format!("/graphql ({repo})"));
        let response: Value = self
            .http
            .post(self.url("/graphql"))
            .bearer_auth(token)
            .json(&json!({"query": query, "variables": variables}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(error) = response["errors"].as_array().and_then(|e| e.first()) {
            bail!("{}", error["message"].as_str().unwrap_or("unknown error"));
        }
        Ok(response)
    }

    /// A GET of the repository's REST API, as JSON.
    async fn repo_json<T: serde::de::DeserializeOwned>(
        &self,
        repo: &str,
        path: &str,
    ) -> anyhow::Result<T> {
        Ok(self
            .repo_get(repo, path, "application/vnd.github+json")
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn repo_post(&self, repo: &str, path: &str, payload: &Value) -> anyhow::Result<Value> {
        self.repo_send(reqwest::Method::POST, repo, path, Some(payload))
            .await
    }

    /// A write to the repository's REST API with its installation's token; GitHub's refusal comes back as an error.
    async fn repo_send(
        &self,
        method: reqwest::Method,
        repo: &str,
        path: &str,
        payload: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let token = self.token_for(repo).await?;
        self.audit(method.as_str(), &format!("/repos/{repo}{path}"));
        let mut request = self
            .http
            .request(method, self.url(&format!("/repos/{repo}{path}")))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json");
        if let Some(payload) = payload {
            request = request.json(&without_agent_credits_in(payload));
        }
        let response = request.send().await?;
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
        self.graphql(
            repo,
            "mutation($id: ID!, $method: PullRequestMergeMethod!, $head: GitObjectID!) { \
             enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method, expectedHeadOid: $head}) \
             { clientMutationId } }",
            json!({"id": pull_node_id, "method": method, "head": expected_head}),
        )
        .await
        .map(|_| ())
    }

    /// An open pull request from `head` into `base`, if there is one, in `create_pull`'s result shape.
    pub async fn open_pull(
        &self,
        repo: &str,
        head: &str,
        base: &str,
    ) -> anyhow::Result<Option<Value>> {
        let pulls: Vec<Value> = self
            .repo_json(
                repo,
                &format!(
                    "/pulls{}",
                    query(&[
                        ("state", "open"),
                        ("head", &head_ref(repo, head)),
                        ("base", base)
                    ])
                ),
            )
            .await?;
        Ok(pulls.first().map(|body| {
            json!({"url": body["html_url"], "number": body["number"], "node_id": body["node_id"]})
        }))
    }

    /// The number of the stack a pull request is in, if any (GitHub's stacked pull requests).
    /// None too when the repository doesn't have stacked pull requests, which GitHub answers with a 404.
    pub async fn stack_of(&self, repo: &str, pull: u64) -> anyhow::Result<Option<u64>> {
        let response = self
            .repo_get(
                repo,
                &format!("/stacks?pull_request={pull}"),
                "application/vnd.github+json",
            )
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let stacks: Vec<Value> = response.error_for_status()?.json().await?;
        Ok(stacks.first().and_then(|stack| stack["number"].as_u64()))
    }

    /// Has a stack merge itself once its required checks pass, through auto-merge on its top pull request, at
    /// `top_sha`. Refused unless the stack holds exactly `pulls`, bottom to top: others in it would merge unreviewed.
    ///
    /// `layers` are each layer's branch and the commit it was proposed at, bottom to top. After turning auto-merge
    /// on, every layer must still be there with no push let through to it since `since`, or it's turned off again:
    /// the push guard only turns off auto-merge that's already on, and a push it let through earlier may land late.
    pub async fn auto_merge_stack(
        &self,
        repo: &str,
        merge: &StackMerge<'_>,
        pushes: &PushLog,
        since: Instant,
    ) -> anyhow::Result<()> {
        let StackMerge {
            stack,
            pulls,
            top_node_id,
            layers,
        } = *merge;
        let members = self.stack_members(repo, stack).await?;
        if members != pulls {
            bail!("stack #{stack} holds other pull requests than these");
        }
        let Some((_, top_sha)) = layers.last() else {
            bail!("the stack has no layers");
        };
        let branches: Vec<&str> = layers.iter().map(|(branch, _)| *branch).collect();
        // The authorizer sees a push start, not land, so a push it let through lately may still be on its way.
        if let Some(branch) = pushes.pushed_since(repo, &branches, since, PUSH_SETTLE) {
            bail!(
                "`{branch}` was pushed to in the last few minutes, and that push may still be landing"
            );
        }
        if let Some(branch) = self.moved_layer(repo, layers).await? {
            bail!("`{branch}` moved since it was proposed");
        }
        self.enable_auto_merge(repo, top_node_id, top_sha).await?;
        // A push let through in the meantime: GitHub can't merge its commits before their required checks pass, so
        // turning auto-merge off again in time is enough. Any doubt turns it off.
        let doubt = match self.moved_layer(repo, layers).await {
            Ok(Some(branch)) => Some(format!("`{branch}` moved while the stack was being opened")),
            Ok(None) => pushes
                .pushed_since(repo, &branches, since, Duration::ZERO)
                .map(|branch| format!("`{branch}` was pushed to while the stack was being opened")),
            Err(e) => Some(format!("its layers couldn't be checked ({e})")),
        };
        let Some(doubt) = doubt else {
            return Ok(());
        };
        match self.disable_pull_auto_merge_in(repo, top_node_id).await {
            Ok(()) => bail!("{doubt}"),
            Err(e) => bail!(
                "{doubt}, and auto-merge couldn't be turned off again ({e}): turn it off by hand"
            ),
        }
    }

    /// The first layer not at the commit it was proposed at.
    async fn moved_layer<'a>(
        &self,
        repo: &str,
        layers: &[(&'a str, &str)],
    ) -> anyhow::Result<Option<&'a str>> {
        for (branch, sha) in layers {
            if self.branch_sha(repo, branch).await? != *sha {
                return Ok(Some(branch));
            }
        }
        Ok(None)
    }

    /// Checks on a stack that's set to merge itself: `Done` once its top pull request is closed, merged or no longer
    /// set to merge, or the number of the first layer whose head moved from the commit it was proposed at, after
    /// turning the top's auto-merge off.
    pub async fn check_merging_stack(
        &self,
        repo: &str,
        top_node_id: &str,
        layers: &[(u64, &str)],
    ) -> anyhow::Result<StackWatch> {
        let Some((top, _)) = layers.last() else {
            return Ok(StackWatch::Done);
        };
        let top_pull = self.pull(repo, *top).await?;
        if top_pull["state"] != "open" || top_pull["auto_merge"].is_null() {
            return Ok(StackWatch::Done);
        }
        // A layer seen to have moved turns auto-merge off even if another couldn't be read: a lookup that fails only
        // matters when nothing moved.
        let mut unread = None;
        for (number, sha) in layers {
            let pull = if number == top {
                top_pull.clone()
            } else {
                match self.pull(repo, *number).await {
                    Ok(pull) => pull,
                    Err(e) => {
                        unread.get_or_insert(e);
                        continue;
                    }
                }
            };
            if pull["state"] == "open" && pull["head"]["sha"].as_str() != Some(*sha) {
                self.disable_pull_auto_merge_in(repo, top_node_id).await?;
                return Ok(StackWatch::Moved(*number));
            }
        }
        unread.map_or(Ok(StackWatch::Holding), Err)
    }

    /// The diff from `base` to the commit `sha`, as of their merge base. A commit, unlike a branch, can't move
    /// between being named and being read.
    pub async fn diff_to(&self, repo: &str, base: &str, sha: &str) -> anyhow::Result<String> {
        let range = format!("/compare/{}...{sha}", branch_in_path(base));
        Ok(self
            .repo_get(repo, &range, "application/vnd.github.diff")
            .await?
            .error_for_status()?
            .text()
            .await?)
    }

    /// A pull request as GitHub's REST API has it: its state, head and base, and node id.
    pub async fn pull(&self, repo: &str, number: u64) -> anyhow::Result<Value> {
        self.repo_json(repo, &format!("/pulls/{number}")).await
    }

    /// Marks a draft pull request, by its node id, ready for review.
    pub async fn mark_ready(&self, repo: &str, node_id: &str) -> anyhow::Result<()> {
        self.graphql(
            repo,
            "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { clientMutationId } }",
            json!({"id": node_id}),
        )
        .await
        .map(|_| ())
    }

    /// A stack's pull requests, bottom to top.
    pub async fn stack_members(&self, repo: &str, stack: u64) -> anyhow::Result<Vec<u64>> {
        let stack: Value = self.repo_json(repo, &format!("/stacks/{stack}")).await?;
        Ok(stack["pull_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pull| pull["number"].as_u64())
            .collect())
    }

    /// Links pull requests, bottom to top, into a stack; returns its number.
    pub async fn link_stack(&self, repo: &str, pulls: &[u64]) -> anyhow::Result<u64> {
        let body = self
            .repo_post(repo, "/stacks", &json!({"pull_requests": pulls}))
            .await?;
        body["number"]
            .as_u64()
            .context("GitHub returned no stack number")
    }

    /// Issues created in the last day, by title, in `create_issue`'s result shape.
    pub async fn recent_issues(&self, repo: &str) -> anyhow::Result<HashMap<String, Value>> {
        let since = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        let since = &since;
        let issues = all_pages(|page| async move {
            self.repo_json(
                repo,
                &format!("/issues?state=all&since={since}&per_page={PAGE}&page={page}"),
            )
            .await
        })
        .await?;
        Ok(issues
            .into_iter()
            .filter(|issue| {
                issue["pull_request"].is_null()
                    && issue["created_at"]
                        .as_str()
                        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                        .is_some_and(|at| at > Utc::now() - chrono::Duration::days(1))
            })
            .filter_map(|issue| {
                let title = issue["title"].as_str()?.to_owned();
                Some((
                    title,
                    json!({"url": issue["html_url"], "number": issue["number"]}),
                ))
            })
            .collect())
    }

    /// An issue's title and body as they are now, `{title, body}`; a missing body is empty.
    pub async fn issue(&self, repo: &str, number: u64) -> anyhow::Result<Value> {
        let issue: Value = self.repo_json(repo, &format!("/issues/{number}")).await?;
        Ok(json!({
            "title": issue["title"].as_str().unwrap_or_default(),
            "body": issue["body"].as_str().unwrap_or_default(),
        }))
    }

    pub async fn create_issue(&self, repo: &str, payload: &Value) -> anyhow::Result<Value> {
        let body = self.repo_post(repo, "/issues", payload).await?;
        Ok(json!({"url": body["html_url"], "number": body["number"]}))
    }

    /// Changes an issue's fields: `title`, `body`, `state`, `state_reason`, as given.
    pub async fn update_issue(&self, repo: &str, number: u64, patch: &Value) -> anyhow::Result<()> {
        self.repo_send(
            reqwest::Method::PATCH,
            repo,
            &format!("/issues/{number}"),
            Some(patch),
        )
        .await
        .map(|_| ())
    }

    pub async fn comment_on_issue(
        &self,
        repo: &str,
        number: u64,
        body: &str,
    ) -> anyhow::Result<()> {
        self.repo_post(
            repo,
            &format!("/issues/{number}/comments"),
            &json!({"body": body}),
        )
        .await
        .map(|_| ())
    }

    /// The bodies of an issue's comments, oldest first.
    pub async fn issue_comments(&self, repo: &str, number: u64) -> anyhow::Result<Vec<String>> {
        let comments = all_pages(|page| async move {
            self.repo_json(
                repo,
                &format!("/issues/{number}/comments?per_page={PAGE}&page={page}"),
            )
            .await
        })
        .await?;
        Ok(comments
            .iter()
            .filter_map(|c| c["body"].as_str().map(str::to_owned))
            .collect())
    }

    pub async fn add_labels(&self, repo: &str, number: u64, labels: &[&str]) -> anyhow::Result<()> {
        self.repo_post(
            repo,
            &format!("/issues/{number}/labels"),
            &json!({"labels": labels}),
        )
        .await
        .map(|_| ())
    }

    /// Takes a label off an issue; one it doesn't have is fine.
    pub async fn remove_label(&self, repo: &str, number: u64, label: &str) -> anyhow::Result<()> {
        let path = format!("/issues/{number}/labels/{}", branch_in_path(label));
        match self
            .repo_send(reqwest::Method::DELETE, repo, &path, None)
            .await
        {
            Err(e) if e.to_string().contains("404") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Creates the labels in `names` that `repo` doesn't have yet.
    pub async fn ensure_labels(&self, repo: &str, names: &[&str]) -> anyhow::Result<()> {
        let mut existing: Vec<String> = all_pages(|page| async move {
            self.repo_json(repo, &format!("/labels?per_page={PAGE}&page={page}"))
                .await
        })
        .await?
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::to_lowercase))
        .collect();
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

/// What GitHub says of a user's personal access token (ADR 28): whose it is, when it expires, and which
/// repositories it reaches.
#[derive(Debug, PartialEq)]
pub struct UserToken {
    pub login: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub repositories: Vec<String>,
    /// More than [`MAX_LISTED_PAGES`] pages of them, so the list stops short.
    pub more_repositories: bool,
}

/// How many pages of a token's repositories are listed.
const MAX_LISTED_PAGES: usize = 10;

/// The `github-authentication-token-expiration` header, e.g. `2026-11-27 12:00:00 UTC` or `… +0100`.
fn token_expiry(header: &str) -> Option<DateTime<Utc>> {
    let header = header.trim();
    DateTime::parse_from_str(header, "%Y-%m-%d %H:%M:%S %z")
        .map(|at| at.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(header.strip_suffix(" UTC")?, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|at| at.and_utc())
        })
}

/// Asks GitHub about a user's token. `Ok(Err(_))` is GitHub refusing it, with a reason for the user.
pub async fn inspect_user_token(
    http: &reqwest::Client,
    api: &str,
    token: &str,
) -> anyhow::Result<Result<UserToken, String>> {
    let get = |path: String| {
        http.get(format!("{api}{path}"))
            .bearer_auth(token)
            .header(reqwest::header::USER_AGENT, "sloppod")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
    };
    let user = get("/user".into()).await?;
    if matches!(
        user.status(),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Ok(Err(
            "GitHub refused the token: it's wrong, expired or revoked".into(),
        ));
    }
    let user = user.error_for_status()?;
    let expires_at = user
        .headers()
        .get("github-authentication-token-expiration")
        .and_then(|h| h.to_str().ok())
        .and_then(token_expiry);
    let login = user.json::<Value>().await?["login"]
        .as_str()
        .context("GitHub's /user has no login")?
        .to_owned();
    let mut repositories = Vec::new();
    let mut more_repositories = false;
    for page in 1..=MAX_LISTED_PAGES + 1 {
        if page > MAX_LISTED_PAGES {
            more_repositories = true;
            break;
        }
        let items: Vec<Value> = get(format!("/user/repos?per_page={PAGE}&page={page}"))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let last = items.len() < PAGE;
        repositories.extend(
            items
                .iter()
                .filter_map(|repo| repo["full_name"].as_str().map(str::to_owned)),
        );
        if last {
            break;
        }
    }
    Ok(Ok(UserToken {
        login,
        expires_at,
        repositories,
        more_repositories,
    }))
}

/// A payload's `body` (a pull request's description, an issue's body, a comment) without agents' credits.
fn without_agent_credits_in(payload: &Value) -> Value {
    let mut payload = payload.clone();
    if let Some(body) = payload.get_mut("body")
        && let Some(text) = body.as_str()
    {
        *body = json!(without_agent_credits(text));
    }
    payload
}

/// What sloppod writes to GitHub carries no credits an agent added for itself ("Co-Authored-By: Claude …",
/// "Generated with Claude Code"), whatever the agent wrote: the owner decides how their work is attributed.
pub fn without_agent_credits(text: &str) -> String {
    let credit = |line: &str| {
        let line = line.trim().to_ascii_lowercase();
        (line.starts_with("co-authored-by:")
            && (line.contains("anthropic.com") || line.contains("claude")))
            || {
                // The footer alone on its line, with or without its emoji and link; not prose about it.
                let footer = line.trim_start_matches('🤖').trim_start();
                footer == "generated with claude code"
                    || footer == "generated with [claude code]"
                    || (footer.starts_with("generated with [claude code](")
                        && footer.ends_with(')'))
            }
    };
    if !text.lines().any(credit) {
        return text.to_owned();
    }
    let kept: Vec<&str> = text.lines().filter(|line| !credit(line)).collect();
    kept.join("\n").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    use super::*;

    /// A fake GitHub for a user's token: `github_pat_good` is octo's, expiring at the end of November, and
    /// reaches `repos` repositories; any other token is refused.
    async fn user_github(repos: usize) -> String {
        async fn user(
            headers: axum::http::HeaderMap,
        ) -> Result<impl axum::response::IntoResponse, StatusCode> {
            if headers.get("authorization").and_then(|h| h.to_str().ok())
                != Some("Bearer github_pat_good")
            {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok((
                [(
                    "github-authentication-token-expiration",
                    "2026-11-30 12:00:00 UTC",
                )],
                Json(json!({"login": "octo"})),
            ))
        }
        let router =
            Router::new().route("/user", get(user)).route(
                "/user/repos",
                get(
                    move |axum::extract::Query(page): axum::extract::Query<
                        HashMap<String, usize>,
                    >| async move {
                        let (per_page, page) = (page["per_page"], page["page"]);
                        let names: Vec<Value> = ((page - 1) * per_page
                            ..(page * per_page).min(repos))
                            .map(|n| json!({"full_name": format!("octo/r{n}")}))
                            .collect();
                        Json(names)
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        api
    }

    #[tokio::test]
    async fn a_user_token_is_known_by_its_account_expiry_and_repositories() {
        let api = user_github(150).await;
        let http = reqwest::Client::new();
        let token = inspect_user_token(&http, &api, "github_pat_good")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(token.login, "octo");
        assert_eq!(
            token.expires_at,
            Some("2026-11-30T12:00:00Z".parse().unwrap())
        );
        assert_eq!(token.repositories.len(), 150);
        assert_eq!(token.repositories[149], "octo/r149");
        assert!(!token.more_repositories);

        let refused = inspect_user_token(&http, &api, "github_pat_revoked")
            .await
            .unwrap();
        assert!(refused.unwrap_err().contains("refused"));

        let many = inspect_user_token(&http, &user_github(5_000).await, "github_pat_good")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(many.repositories.len(), MAX_LISTED_PAGES * PAGE);
        assert!(many.more_repositories);
    }

    #[test]
    fn reads_a_tokens_expiry() {
        let at: DateTime<Utc> = "2026-11-30T12:00:00Z".parse().unwrap();
        assert_eq!(token_expiry("2026-11-30 12:00:00 UTC"), Some(at));
        assert_eq!(token_expiry("2026-11-30 13:00:00 +0100"), Some(at));
        assert_eq!(token_expiry("soon"), None);
    }

    /// Installation of `o/other`, which never changes.
    const OTHER: u64 = 10;

    /// A fake GitHub: `o/r` and `o/r2` are on the current installation, `o/other` on [`OTHER`], other
    /// repositories have none.
    #[derive(Default)]
    struct Fake {
        installation: AtomicU64,
        lookups: AtomicUsize,
        mints: AtomicUsize,
        /// Each mint's installation and request body.
        minted: Mutex<Vec<(u64, Value)>>,
        /// Holds each mint until this many are in flight at once.
        rendezvous: Option<tokio::sync::Barrier>,
        /// Open pull requests of `o/r`: number, head branch, and whether auto-merge is on.
        pulls: Mutex<Vec<(u64, &'static str, bool)>>,
        /// Stacks of `o/r` by number, with their pull requests bottom to top; None for a repository without them.
        stacks: Option<Vec<(u64, Vec<u64>)>>,
        /// Branch heads of `o/r`.
        heads: Mutex<HashMap<String, String>>,
        /// Head lookups fail while any pull request has auto-merge on.
        heads_fail_once_merging: bool,
        /// Pull requests of `o/r` that are merged.
        merged: Mutex<Vec<u64>>,
        /// Pull requests whose lookup fails.
        failing: Mutex<Vec<u64>>,
        /// Node ids of the pull requests marked ready for review.
        readied: Mutex<Vec<String>>,
    }

    async fn commit(
        State(fake): State<Arc<Fake>>,
        Path((_, _, reference)): Path<(String, String, String)>,
    ) -> Result<Json<Value>, StatusCode> {
        if fake.heads_fail_once_merging && fake.pulls.lock().unwrap().iter().any(|p| p.2) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let heads = fake.heads.lock().unwrap();
        heads
            .get(&reference)
            .map(|sha| Json(json!({"sha": sha})))
            .ok_or(StatusCode::NOT_FOUND)
    }

    fn pull_json(&(number, head, auto_merge): &(u64, &str, bool)) -> Value {
        json!({
            "number": number,
            "state": "open",
            "node_id": format!("PR{number}"),
            "head": {"ref": head},
            "auto_merge": if auto_merge { json!({"merge_method": "squash"}) } else { Value::Null },
        })
    }

    async fn pulls(
        State(fake): State<Arc<Fake>>,
        axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let head = query
            .get("head")
            .and_then(|h| h.split_once(':'))
            .map(|(_, b)| b.to_owned());
        let pulls = fake.pulls.lock().unwrap();
        Json(json!(
            pulls
                .iter()
                .filter(|p| head.as_deref().is_none_or(|h| h == p.1))
                .map(pull_json)
                .collect::<Vec<_>>()
        ))
    }

    async fn pull(
        State(fake): State<Arc<Fake>>,
        Path((_, _, number)): Path<(String, String, u64)>,
    ) -> Result<Json<Value>, StatusCode> {
        if fake.failing.lock().unwrap().contains(&number) {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        let mut pull = {
            let pulls = fake.pulls.lock().unwrap();
            pulls
                .iter()
                .find(|p| p.0 == number)
                .map(pull_json)
                .ok_or(StatusCode::NOT_FOUND)?
        };
        let branch = pull["head"]["ref"].as_str().unwrap_or_default().to_owned();
        if let Some(sha) = fake.heads.lock().unwrap().get(&branch) {
            pull["head"]["sha"] = json!(sha);
        }
        if fake.merged.lock().unwrap().contains(&number) {
            pull["state"] = json!("closed");
        }
        Ok(Json(pull))
    }

    fn stack_json(number: u64, members: &[u64]) -> Value {
        json!({"number": number, "pull_requests": members.iter().map(|n| json!({"number": n})).collect::<Vec<_>>()})
    }

    async fn stacks(
        State(fake): State<Arc<Fake>>,
        axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, StatusCode> {
        let stacks = fake.stacks.as_ref().ok_or(StatusCode::NOT_FOUND)?;
        let pull: Option<u64> = query.get("pull_request").and_then(|p| p.parse().ok());
        Ok(Json(json!(
            stacks
                .iter()
                .filter(|(_, members)| pull.is_none_or(|p| members.contains(&p)))
                .map(|(number, members)| stack_json(*number, members))
                .collect::<Vec<_>>()
        )))
    }

    async fn stack(
        State(fake): State<Arc<Fake>>,
        Path((_, _, number)): Path<(String, String, u64)>,
    ) -> Result<Json<Value>, StatusCode> {
        let stacks = fake.stacks.as_ref().ok_or(StatusCode::NOT_FOUND)?;
        stacks
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(n, members)| Json(stack_json(*n, members)))
            .ok_or(StatusCode::NOT_FOUND)
    }

    /// #1 has a body; #2 has none, which GitHub gives as null.
    async fn issue(
        Path((_, _, number)): Path<(String, String, u64)>,
    ) -> Result<Json<Value>, StatusCode> {
        match number {
            1 => Ok(Json(
                json!({"number": 1, "title": "Flaky", "body": "It fails.\n"}),
            )),
            2 => Ok(Json(json!({"number": 2, "title": "Bare", "body": null}))),
            _ => Err(StatusCode::NOT_FOUND),
        }
    }

    async fn repository() -> Json<Value> {
        Json(json!({"allow_squash_merge": true}))
    }

    /// Turns auto-merge on or off for the pull request whose node id the mutation names.
    async fn graphql(State(fake): State<Arc<Fake>>, Json(body): Json<Value>) -> Json<Value> {
        let on = body["query"]
            .as_str()
            .unwrap_or_default()
            .contains("enablePullRequestAutoMerge");
        let id = body["variables"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if body["query"]
            .as_str()
            .unwrap_or_default()
            .contains("markPullRequestReadyForReview")
        {
            fake.readied.lock().unwrap().push(id);
            return Json(json!({"data": {}}));
        }
        for pull in fake.pulls.lock().unwrap().iter_mut() {
            if format!("PR{}", pull.0) == id {
                pull.2 = on;
            }
        }
        Json(json!({"data": {}}))
    }

    async fn installation(
        State(fake): State<Arc<Fake>>,
        Path((owner, name)): Path<(String, String)>,
    ) -> Result<Json<Value>, StatusCode> {
        fake.lookups.fetch_add(1, Ordering::SeqCst);
        let id = match (owner.to_lowercase().as_str(), name.to_lowercase().as_str()) {
            ("o", "r" | "r2") => fake.installation.load(Ordering::SeqCst),
            ("o", "other") => OTHER,
            _ => return Err(StatusCode::NOT_FOUND),
        };
        let permissions = json!({
            "contents": "write",
            "issues": "write",
            "metadata": "read",
            "pull_requests": "write",
            "workflows": "write",
        });
        Ok(Json(json!({"id": id, "permissions": permissions})))
    }

    async fn mint(
        State(fake): State<Arc<Fake>>,
        Path(id): Path<u64>,
        Json(body): Json<Value>,
    ) -> Result<Json<Value>, StatusCode> {
        fake.mints.fetch_add(1, Ordering::SeqCst);
        fake.minted.lock().unwrap().push((id, body));
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
            .route("/repos/{owner}/{name}", get(repository))
            .route("/repos/{owner}/{name}/pulls", get(pulls))
            .route("/repos/{owner}/{name}/pulls/{number}", get(pull))
            .route("/repos/{owner}/{name}/stacks", get(stacks))
            .route("/repos/{owner}/{name}/stacks/{number}", get(stack))
            .route("/repos/{owner}/{name}/commits/{reference}", get(commit))
            .route("/repos/{owner}/{name}/issues/{number}", get(issue))
            .route("/graphql", post(graphql))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256).unwrap();
        let app = GitHubApp::with_api(&api, "1".into(), key.serialize_pem().as_bytes()).unwrap();
        (fake, app)
    }

    /// The stack #7 of three layers, the top one set to merge the stack.
    fn stacked() -> Fake {
        Fake {
            pulls: Mutex::new(vec![
                (1, "s-1", false),
                (2, "s-2", false),
                (3, "s-3", true),
                (4, "other", true),
            ]),
            stacks: Some(vec![(7, vec![1, 2, 3])]),
            ..Fake::default()
        }
    }

    fn auto_merging(fake: &Fake) -> Vec<u64> {
        fake.pulls
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.2)
            .map(|p| p.0)
            .collect()
    }

    #[tokio::test]
    async fn reads_an_issue_as_it_is_now() {
        let (_, app) = fake_github().await;
        assert_eq!(
            app.issue("o/r", 1).await.unwrap(),
            json!({"title": "Flaky", "body": "It fails.\n"})
        );
        assert_eq!(
            app.issue("o/r", 2).await.unwrap(),
            json!({"title": "Bare", "body": ""})
        );
        assert!(app.issue("o/r", 3).await.is_err());
    }

    #[tokio::test]
    async fn marks_a_draft_ready_for_review_by_its_node_id() {
        let (fake, app) = serve(stacked()).await;
        app.mark_ready("o/r", "PR2").await.unwrap();
        assert_eq!(*fake.readied.lock().unwrap(), ["PR2"]);
        // Auto-merge isn't touched.
        assert_eq!(auto_merging(&fake), [3, 4]);
    }

    #[tokio::test]
    async fn a_push_to_any_layer_stops_the_stack_merging() {
        let (fake, app) = serve(stacked()).await;
        assert_eq!(app.disable_auto_merge("o/r", "s-1").await.unwrap(), [3]);
        assert_eq!(auto_merging(&fake), [4]);
    }

    #[tokio::test]
    async fn a_push_without_stacks_stops_its_own_pull_request() {
        let fake = Fake {
            pulls: Mutex::new(vec![(1, "s-1", true), (4, "other", true)]),
            stacks: None,
            ..Fake::default()
        };
        let (fake, app) = serve(fake).await;
        assert_eq!(app.disable_auto_merge("o/r", "s-1").await.unwrap(), [1]);
        assert_eq!(auto_merging(&fake), [4]);
    }

    #[tokio::test]
    async fn a_push_finds_its_pull_request_whatever_the_branch_is_called() {
        // Session branches can't have these characters, but the lookup must still ask about the branch pushed: an
        // unencoded `+` reads as a space and a `#` ends the URL, so the pull request would be missed.
        let fake = Fake {
            pulls: Mutex::new(vec![
                (1, "a+b", true),
                (2, "a b", true),
                (3, "c#d", true),
                (4, "c", true),
            ]),
            stacks: None,
            ..Fake::default()
        };
        let (fake, app) = serve(fake).await;
        assert_eq!(app.disable_auto_merge("o/r", "a+b").await.unwrap(), [1]);
        assert_eq!(app.disable_auto_merge("o/r", "c#d").await.unwrap(), [3]);
        assert_eq!(auto_merging(&fake), [2, 4]);
    }

    /// The unmerged stack #7 of three layers, each at the commit it was proposed at.
    fn unmerged() -> Fake {
        Fake {
            pulls: Mutex::new(vec![
                (1, "s-1", false),
                (2, "s-2", false),
                (3, "s-3", false),
            ]),
            stacks: Some(vec![(7, vec![1, 2, 3])]),
            heads: Mutex::new(
                [("s-1", "a"), ("s-2", "b"), ("s-3", "c")]
                    .into_iter()
                    .map(|(b, s)| (b.to_owned(), s.to_owned()))
                    .collect(),
            ),
            ..Fake::default()
        }
    }

    const LAYERS: &[(&str, &str)] = &[("s-1", "a"), ("s-2", "b"), ("s-3", "c")];

    fn merge<'a>(pulls: &'a [u64]) -> StackMerge<'a> {
        StackMerge {
            stack: 7,
            pulls,
            top_node_id: "PR3",
            layers: LAYERS,
        }
    }

    #[tokio::test]
    async fn a_stack_merges_only_with_exactly_its_proposed_layers() {
        let (fake, app) = serve(unmerged()).await;
        let pushes = PushLog::default();
        let since = Instant::now();
        // The upper two of an existing three-layer stack.
        assert!(
            app.auto_merge_stack("o/r", &merge(&[2, 3]), &pushes, since)
                .await
                .is_err()
        );
        assert!(auto_merging(&fake).is_empty());
        app.auto_merge_stack("o/r", &merge(&[1, 2, 3]), &pushes, since)
            .await
            .unwrap();
        assert_eq!(auto_merging(&fake), [3]);
    }

    #[tokio::test]
    async fn a_layer_that_moved_meanwhile_stops_the_stack_merging() {
        let (fake, app) = serve(unmerged()).await;
        let since = Instant::now();
        fake.heads.lock().unwrap().insert("s-1".into(), "a2".into());
        let refused = app
            .auto_merge_stack("o/r", &merge(&[1, 2, 3]), &PushLog::default(), since)
            .await;
        assert!(refused.unwrap_err().to_string().contains("s-1"));
        assert!(auto_merging(&fake).is_empty());
    }

    #[tokio::test]
    async fn a_push_let_through_meanwhile_stops_the_stack_merging() {
        let (fake, app) = serve(unmerged()).await;
        let pushes = PushLog::default();
        let since = Instant::now();
        // Let through after the stack started opening, its commits not on the branch yet.
        pushes.admit("O/R", "s-2");
        let refused = app
            .auto_merge_stack("o/r", &merge(&[1, 2, 3]), &pushes, since)
            .await;
        assert!(refused.unwrap_err().to_string().contains("s-2"));
        assert!(auto_merging(&fake).is_empty());
    }

    #[tokio::test]
    async fn a_recent_push_that_may_still_land_keeps_auto_merge_off() {
        let (fake, app) = serve(unmerged()).await;
        let pushes = PushLog::default();
        // Let through before the stack started opening, and maybe still landing.
        pushes.admit("o/r", "s-1");
        let since = Instant::now();
        let refused = app
            .auto_merge_stack("o/r", &merge(&[1, 2, 3]), &pushes, since)
            .await;
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("may still be landing")
        );
        assert!(auto_merging(&fake).is_empty());

        // Long enough ago, with every layer at its proposed commit, it's fine.
        let settled = since + PUSH_SETTLE + Duration::from_secs(1);
        app.auto_merge_stack("o/r", &merge(&[1, 2, 3]), &pushes, settled)
            .await
            .unwrap();
        assert_eq!(auto_merging(&fake), [3]);
    }

    #[tokio::test]
    async fn a_failed_check_after_turning_auto_merge_on_turns_it_off() {
        let fake = Fake {
            heads_fail_once_merging: true,
            ..unmerged()
        };
        let (fake, app) = serve(fake).await;
        let refused = app
            .auto_merge_stack(
                "o/r",
                &merge(&[1, 2, 3]),
                &PushLog::default(),
                Instant::now(),
            )
            .await;
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("couldn't be checked")
        );
        assert!(auto_merging(&fake).is_empty());
    }

    const WATCHED: &[(u64, &str)] = &[(1, "a"), (2, "b"), (3, "c")];

    /// The stack #7, its top set to merge it.
    fn merging() -> Fake {
        let fake = unmerged();
        fake.pulls.lock().unwrap()[2].2 = true;
        fake
    }

    #[tokio::test]
    async fn a_merging_stack_holds_while_its_layers_do() {
        let (_, app) = serve(merging()).await;
        assert_eq!(
            app.check_merging_stack("o/r", "PR3", WATCHED)
                .await
                .unwrap(),
            StackWatch::Holding
        );
    }

    #[tokio::test]
    async fn a_moved_layer_stops_the_stack_merging() {
        let (fake, app) = serve(merging()).await;
        fake.heads.lock().unwrap().insert("s-1".into(), "a2".into());
        assert_eq!(
            app.check_merging_stack("o/r", "PR3", WATCHED)
                .await
                .unwrap(),
            StackWatch::Moved(1)
        );
        assert!(auto_merging(&fake).is_empty());
    }

    #[tokio::test]
    async fn a_moved_layer_stops_the_stack_merging_though_another_cant_be_read() {
        let (fake, app) = serve(merging()).await;
        fake.heads.lock().unwrap().insert("s-2".into(), "b2".into());
        fake.failing.lock().unwrap().push(1);
        assert_eq!(
            app.check_merging_stack("o/r", "PR3", WATCHED)
                .await
                .unwrap(),
            StackWatch::Moved(2)
        );
        assert!(auto_merging(&fake).is_empty());

        // With nothing seen to move, an unread layer is an error, and auto-merge stays on until the next look.
        let (fake, app) = serve(merging()).await;
        fake.failing.lock().unwrap().push(1);
        assert!(
            app.check_merging_stack("o/r", "PR3", WATCHED)
                .await
                .is_err()
        );
        assert_eq!(auto_merging(&fake), [3]);
    }

    #[tokio::test]
    async fn watching_ends_once_the_stack_merged_or_stopped_merging() {
        let (fake, app) = serve(merging()).await;
        fake.merged.lock().unwrap().extend([1, 2, 3]);
        assert_eq!(
            app.check_merging_stack("o/r", "PR3", WATCHED)
                .await
                .unwrap(),
            StackWatch::Done
        );

        let (fake, app) = serve(unmerged()).await;
        fake.heads.lock().unwrap().insert("s-1".into(), "a2".into());
        assert_eq!(
            app.check_merging_stack("o/r", "PR3", WATCHED)
                .await
                .unwrap(),
            StackWatch::Done
        );
    }

    #[tokio::test]
    async fn reads_every_page_of_a_list() {
        let pages = [PAGE, PAGE, 5];
        let fetched = std::sync::Mutex::new(Vec::new());
        let items = all_pages(|page| {
            fetched.lock().unwrap().push(page);
            async move { Ok(vec![json!(page); pages[page - 1]]) }
        })
        .await
        .unwrap();
        assert_eq!(items.len(), 2 * PAGE + 5);
        assert_eq!(*fetched.lock().unwrap(), [1, 2, 3]);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_mint() {
        let (fake, app) = fake_github().await;
        let tokens = futures::future::join_all((0..10).map(|_| app.token_for("o/r"))).await;
        assert!(tokens.iter().all(|t| t.as_deref().ok() == Some("token-1")));
        assert_eq!(fake.mints.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn branches_keep_their_meaning_in_urls() {
        assert_eq!(
            branch_in_path("sloppod/abcd1234-v1.2_x"),
            "sloppod/abcd1234-v1.2_x"
        );
        assert_eq!(branch_in_path("a#b+c d"), "a%23b%2Bc%20d");
        assert_eq!(
            query(&[("head", &head_ref("o/r", "a#b")), ("base", "m&x=1")]),
            "?head=o%3Aa%23b&base=m%26x%3D1"
        );
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

    #[tokio::test]
    async fn session_tokens_cover_only_their_grants() {
        let (fake, app) = fake_github().await;
        app.read_token("O/R", &["O/R", "o/other", "o/r2"])
            .await
            .unwrap();
        app.push_token("o/r").await.unwrap();
        // Same scope as the first read, so it comes from the cache.
        app.read_token("o/r", &["o/r2", "o/r"]).await.unwrap();
        let minted = fake.minted.lock().unwrap().clone();
        assert_eq!(
            minted,
            vec![
                (
                    1,
                    json!({
                        "repositories": ["r", "r2"],
                        "permissions": {
                            "contents": "read",
                            "issues": "read",
                            "metadata": "read",
                            "pull_requests": "read",
                        },
                    })
                ),
                (
                    1,
                    json!({
                        "repositories": ["r"],
                        "permissions": {"contents": "write", "metadata": "read", "workflows": "write"},
                    })
                ),
            ]
        );
    }

    #[test]
    fn pushes_may_change_workflows_only_where_the_installation_can() {
        let installation = |workflows: Option<&str>| {
            let mut permissions = BTreeMap::from([
                ("contents".to_owned(), "write".to_owned()),
                ("metadata".to_owned(), "read".to_owned()),
            ]);
            if let Some(level) = workflows {
                permissions.insert("workflows".to_owned(), level.to_owned());
            }
            permissions
        };
        let contents = BTreeMap::from([
            ("contents".to_owned(), "write".to_owned()),
            ("metadata".to_owned(), "read".to_owned()),
        ]);
        let mut with_workflows = contents.clone();
        with_workflows.insert("workflows".to_owned(), "write".to_owned());
        assert_eq!(
            push_permissions(&installation(Some("write"))),
            with_workflows
        );
        assert_eq!(push_permissions(&installation(None)), contents);
        assert_eq!(push_permissions(&installation(Some("read"))), contents);
    }

    #[tokio::test]
    async fn runtime_tokens_have_the_whole_installation() {
        let (fake, app) = fake_github().await;
        app.token_for("o/r").await.unwrap();
        assert_eq!(*fake.minted.lock().unwrap(), vec![(1, json!({}))]);
    }

    #[tokio::test]
    async fn a_client_acting_as_a_user_uses_their_token_for_everything() {
        let user =
            GitHubApp::for_user_with_api("http://127.0.0.1:9", "github_pat_x".into(), "ada".into())
                .unwrap();
        assert_eq!(user.user_login(), Some("ada"));
        assert_eq!(user.token_for("o/r").await.unwrap(), "github_pat_x");
        assert_eq!(
            user.read_token("o/r", &["o/s"]).await.unwrap(),
            "github_pat_x"
        );
        assert_eq!(user.push_token("o/r").await.unwrap(), "github_pat_x");
        assert_eq!(user.identity().await.unwrap().name, "ada");
        assert!(user.jwt().is_err(), "it never acts as the App");
    }

    #[test]
    fn agents_credits_are_left_out() {
        let text = "Fixes the build.\n\nCo-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n";
        assert_eq!(without_agent_credits(text), "Fixes the build.");
        let plain = "Done.\nGenerated with Claude Code";
        assert_eq!(without_agent_credits(plain), "Done.");
        let kept = "Co-authored-by: Ada <ada@example.com>\nThanks to Claude Shannon.\nThe parser rejects files generated with \
                    Claude Code; steps follow.\n- It drops \"Generated with Claude Code\" lines.\n";
        assert_eq!(without_agent_credits(kept), kept);
        assert_eq!(
            without_agent_credits_in(
                &json!({"title": "T", "body": "B\nCo-authored-by: Claude <noreply@anthropic.com>"})
            ),
            json!({"title": "T", "body": "B"})
        );
    }
}
