//! Credentials the runtime uses on a user's behalf, stored encrypted (ADR 21).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_contracts::{SessionContext, SessionRole};
use sloppod_protocol::api::{Credential, NewCredential};
use sqlx::{FromRow, PgPool};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::github::GitHubApp;
use crate::memini::MeminiAdmin;
use crate::vault::Vault;

pub const CLAUDE: &str = "claude";
pub const GITHUB_APP: &str = "github-app";
pub const MEMINI: &str = "memini";
/// A ChatGPT subscription's Codex login, for review sessions (ADR 24).
pub const CHATGPT: &str = "chatgpt";
/// A user's fine-grained GitHub token, for acting as them where they grant it (ADR 28). Nothing uses it yet.
pub const GITHUB_USER: &str = "github-user";
/// A token an owner-defined capability's server takes (ADR 2, 2026-10-29): sent as `Authorization: Bearer <token>`,
/// or as the value of the header its config names.
pub const TOKEN: &str = "token";
const KINDS: &[&str] = &[CLAUDE, GITHUB_APP, MEMINI, CHATGPT, GITHUB_USER, TOKEN];

/// Codex's OAuth client and token endpoint, which refresh a ChatGPT login.
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// A ChatGPT login's tokens, as stored. Refresh tokens are single-use, so the runtime is their only holder.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatGptTokens {
    pub access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    pub account_id: String,
}

/// Leaves the tokens out, so logging them can't leak them.
impl std::fmt::Debug for ChatGptTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptTokens")
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

impl ChatGptTokens {
    /// The tokens from the `auth.json` that `codex login` writes.
    fn from_auth_json(text: &str) -> Option<Self> {
        let auth: Value = serde_json::from_str(text).ok()?;
        let tokens = &auth["tokens"];
        let field = |name: &str| {
            tokens[name]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            access_token: field("access_token")?,
            refresh_token: field("refresh_token")?,
            id_token: field("id_token"),
            account_id: field("account_id")?,
        })
    }

    /// Whether the access token expires within five minutes, judging by its JWT `exp`; tokens that don't say
    /// are left alone.
    fn expires_soon(&self) -> bool {
        use base64::Engine;
        let exp = self
            .access_token
            .split('.')
            .nth(1)
            .and_then(|payload| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(payload)
                    .ok()
            })
            .and_then(|json| serde_json::from_slice::<Value>(&json).ok())
            .and_then(|claims| claims["exp"].as_i64());
        exp.is_some_and(|exp| exp - Utc::now().timestamp() < 300)
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct CredentialRow {
    pub id: Uuid,
    pub owner: String,
    pub kind: String,
    pub name: String,
    pub config: Value,
    pub secret: Vec<u8>,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
}

impl CredentialRow {
    pub fn to_api(&self) -> Credential {
        Credential {
            id: self.id,
            kind: self.kind.clone(),
            name: self.name.clone(),
            config: self.config.clone(),
            default: self.is_default,
            created_at: self.created_at.to_rfc3339(),
        }
    }

    pub fn config(&self, key: &str) -> Option<&str> {
        self.config
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// The account a session's model grant acts with.
pub enum Account {
    /// Still sealed.
    Found(CredentialRow),
    /// Every account the grant names was deleted since: it's refused until the owner gives it another (ADR 29).
    Gone,
    /// The owner has none of the kind.
    None,
}

/// A credential request that can't be stored, with a reason for the user.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Invalid(pub String);

const COLUMNS: &str = "id, owner, kind, name, config, secret, is_default, created_at";

pub struct Credentials {
    db: PgPool,
    vault: Vault,
    github: Mutex<HashMap<Uuid, Arc<GitHubApp>>>,
    /// Held per login while refreshing it, so a single-use refresh token is only ever spent once, and a slow refresh
    /// holds up only that login's requests.
    chatgpt_refresh: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    /// The account last attached to each session's requests, by session and kind: what a quota report or a failed
    /// turn is about, whichever account would be chosen now (#252). Lost on a restart, when the choice stands in.
    attached: std::sync::Mutex<HashMap<(String, String), Uuid>>,
    http: reqwest::Client,
}

fn has_userinfo(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| !u.username().is_empty() || u.password().is_some())
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// The repository a `github-user` token reaches that's `wanted`, as GitHub names it: one it listed when it was added,
/// or, when it listed more than were read, any well-formed name, which GitHub then decides on when it's used.
pub fn token_reaches(row: &CredentialRow, wanted: &str) -> Option<String> {
    let listed: Vec<&str> = row.config["repositories"]
        .as_array()
        .map(|repos| repos.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match sloppod_contracts::find_repo(listed, wanted) {
        Some(repo) => Some(repo.to_owned()),
        None if row.config["more_repositories"].as_bool() == Some(true)
            && sloppod_contracts::is_repo_name(wanted) =>
        {
            Some(wanted.to_owned())
        }
        None => None,
    }
}

/// Whether a token is one a `github-user` credential can hold: a fine-grained one, which can be narrowed to
/// repositories, and not a classic one, which reaches everything its user can.
fn user_token_shape(token: &str) -> Result<(), &'static str> {
    if token.starts_with("ghp_") {
        Err(
            "that's a classic token, which can't be narrowed to repositories; make a fine-grained one instead",
        )
    } else if !token.starts_with("github_pat_") {
        Err("expected a fine-grained personal access token, which starts with github_pat_")
    } else {
        Ok(())
    }
}

/// Serialises changes to which of an owner's credentials of a kind is the default, until the transaction ends.
async fn lock_kind(tx: &mut sqlx::PgConnection, owner: &str, kind: &str) -> sqlx::Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtext($1 || '/' || $2))")
        .bind(owner)
        .bind(kind)
        .execute(tx)
        .await
        .map(|_| ())
}

impl Credentials {
    pub async fn new(db: PgPool, vault: Vault) -> anyhow::Result<Self> {
        Ok(Self {
            db,
            vault,
            github: Mutex::default(),
            chatgpt_refresh: Mutex::default(),
            attached: Default::default(),
            // The refresh runs while a session's request waits on the authorizer, which Envoy gives 10 s.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?,
        })
    }

    pub async fn list(&self, owner: &str) -> sqlx::Result<Vec<CredentialRow>> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where owner = $1 order by kind, created_at"
        )))
        .bind(owner)
        .fetch_all(&self.db)
        .await
    }

    pub async fn create(
        &self,
        owner: &str,
        new: NewCredential,
    ) -> anyhow::Result<Result<CredentialRow, Invalid>> {
        let name = new.name.trim().to_owned();
        let mut secret = new.secret.trim().to_owned();
        let config = |key: &str| {
            new.config
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
        };
        if !KINDS.contains(&new.kind.as_str()) {
            return Ok(Err(Invalid(format!(
                "unknown kind {}; expected one of {}",
                new.kind,
                KINDS.join(", ")
            ))));
        }
        if name.is_empty() || secret.is_empty() {
            return Ok(Err(Invalid("a name and a secret are required".into())));
        }
        let stored_config = match new.kind.as_str() {
            GITHUB_APP => {
                let app_id = config("app_id");
                if app_id.is_empty() || !app_id.chars().all(|c| c.is_ascii_digit()) {
                    return Ok(Err(Invalid("a GitHub App needs its numeric app_id".into())));
                }
                if let Err(e) = GitHubApp::new(app_id.to_owned(), secret.as_bytes()) {
                    return Ok(Err(Invalid(format!(
                        "the private key doesn't parse: {e:#}"
                    ))));
                }
                serde_json::json!({"app_id": app_id})
            }
            MEMINI => {
                let url = config("url").trim_end_matches('/');
                if host_of(url).is_none() {
                    return Ok(Err(Invalid(
                        "memini needs its base URL, e.g. https://memini.example".into(),
                    )));
                }
                // The config is shown back by the API, unlike the secret.
                if has_userinfo(url) {
                    return Ok(Err(Invalid(
                        "memini's URL can't hold a username or password; the admin key goes in the secret".into(),
                    )));
                }
                let mut stored = serde_json::json!({"url": url});
                if !config("home").is_empty() {
                    stored["home"] = Value::from(config("home"));
                }
                stored
            }
            CHATGPT => {
                let Some(tokens) = ChatGptTokens::from_auth_json(&secret) else {
                    return Ok(Err(Invalid(
                        "paste the whole ~/.codex/auth.json from a `codex login` made just for sloppod".into(),
                    )));
                };
                secret = serde_json::to_string(&tokens)?;
                serde_json::json!({"account_id": tokens.account_id})
            }
            GITHUB_USER => {
                if let Err(why) = user_token_shape(&secret) {
                    return Ok(Err(Invalid(why.into())));
                }
                let token = match crate::github::inspect_user_token(
                    &self.http,
                    crate::github::API,
                    &secret,
                )
                .await?
                {
                    Ok(token) => token,
                    Err(why) => return Ok(Err(Invalid(why))),
                };
                serde_json::json!({
                    "login": token.login,
                    "expires_at": token.expires_at.map(|at| at.to_rfc3339()),
                    "repositories": token.repositories,
                    "more_repositories": token.more_repositories,
                })
            }
            TOKEN => match config("header") {
                "" => serde_json::json!({}),
                header
                    if header
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-') =>
                {
                    serde_json::json!({"header": header.to_ascii_lowercase()})
                }
                _ => {
                    return Ok(Err(Invalid(
                        "a header name is letters, digits and dashes".into(),
                    )));
                }
            },
            _ => serde_json::json!({}),
        };
        let sealed = self.vault.encrypt(secret.as_bytes())?;
        let mut tx = self.db.begin().await?;
        lock_kind(&mut tx, owner, &new.kind).await?;
        let row: Option<CredentialRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "insert into credentials (id, owner, kind, name, config, secret, is_default) \
             values ($1, $2, $3, $4, $5, $6, not exists (select 1 from credentials where owner = $2 and kind = $3 and is_default)) \
             on conflict (owner, kind, name) do nothing returning {COLUMNS}"
        )))
        .bind(Uuid::now_v7())
        .bind(owner)
        .bind(&new.kind)
        .bind(&name)
        .bind(&stored_config)
        .bind(&sealed)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(Err(Invalid(format!(
                "there's already a {} credential named {name}",
                new.kind
            ))));
        };
        Ok(Ok(row))
    }

    pub async fn set_default(&self, owner: &str, id: Uuid) -> sqlx::Result<Option<CredentialRow>> {
        let mut tx = self.db.begin().await?;
        let row: Option<CredentialRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where id = $1 and owner = $2"
        )))
        .bind(id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else { return Ok(None) };
        lock_kind(&mut tx, owner, &row.kind).await?;
        // Two steps: the index allowing one default per kind is checked row by row, so one statement swapping it
        // could briefly have two.
        sqlx::query(
            "update credentials set is_default = false where owner = $1 and kind = $2 and is_default and id <> $3",
        )
        .bind(owner)
        .bind(&row.kind)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update credentials set is_default = true where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(CredentialRow {
            is_default: true,
            ..row
        }))
    }

    /// Deletes a credential; another of its kind becomes the default if it was.
    pub async fn delete(&self, owner: &str, id: Uuid) -> anyhow::Result<bool> {
        let mut tx = self.db.begin().await?;
        let kind: Option<(String,)> =
            sqlx::query_as("select kind from credentials where id = $1 and owner = $2")
                .bind(id)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((kind,)) = kind else {
            return Ok(false);
        };
        lock_kind(&mut tx, owner, &kind).await?;
        let deleted = sqlx::query("delete from credentials where id = $1 and owner = $2")
            .bind(id)
            .bind(owner)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        // The oldest left becomes the default, unless one already is (made so while this waited for the lock).
        sqlx::query(
            "update credentials set is_default = true where id = \
             (select id from credentials where owner = $1 and kind = $2 order by created_at limit 1) \
             and not exists (select 1 from credentials where owner = $1 and kind = $2 and is_default)",
        )
        .bind(owner)
        .bind(&kind)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if !deleted {
            return Ok(false);
        }
        self.github.lock().await.remove(&id);
        Ok(true)
    }

    /// Whether the owner has a credential of a kind, without decrypting it.
    pub async fn has_default(&self, owner: &str, kind: &str) -> sqlx::Result<bool> {
        Ok(self.default_row(owner, kind).await?.is_some())
    }

    /// Every owner's credentials of a kind, still sealed.
    pub async fn of_kind(&self, kind: &str) -> sqlx::Result<Vec<CredentialRow>> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where kind = $1 order by owner, created_at"
        )))
        .bind(kind)
        .fetch_all(&self.db)
        .await
    }

    /// The owner's default credential of a kind, still sealed.
    pub async fn default_row(
        &self,
        owner: &str,
        kind: &str,
    ) -> sqlx::Result<Option<CredentialRow>> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where owner = $1 and kind = $2 and is_default"
        )))
        .bind(owner)
        .bind(kind)
        .fetch_optional(&self.db)
        .await
    }

    /// The owner's default credential of a kind, with its decrypted secret.
    pub async fn secret(
        &self,
        owner: &str,
        kind: &str,
    ) -> anyhow::Result<Option<(CredentialRow, String)>> {
        let Some(row) = self.default_row(owner, kind).await? else {
            return Ok(None);
        };
        let secret = self.vault.decrypt_string(&row.secret)?;
        Ok(Some((row, secret)))
    }

    /// [`Self::account`], for a request it's attached to: kept as the account the session last used.
    pub async fn attach(&self, session: &SessionContext, kind: &str) -> sqlx::Result<Account> {
        let account = self.account(session, kind).await?;
        if let Account::Found(row) = &account {
            self.attached
                .lock()
                .unwrap()
                .insert((session.session_id.clone(), kind.to_owned()), row.id);
        }
        Ok(account)
    }

    /// The account a session last used for a kind: the one last attached to its requests, if it's still there,
    /// otherwise the one it would use now.
    pub async fn used(
        &self,
        session: &SessionContext,
        kind: &str,
    ) -> sqlx::Result<Option<CredentialRow>> {
        let attached = self
            .attached
            .lock()
            .unwrap()
            .get(&(session.session_id.clone(), kind.to_owned()))
            .copied();
        if let Some(id) = attached
            && let Some(row) = self.owned(&session.owner, kind, &id.to_string()).await?
        {
            return Ok(Some(row));
        }
        Ok(match self.account(session, kind).await? {
            Account::Found(row) => Some(row),
            _ => None,
        })
    }

    /// The account a session's model grant acts with (ADR 29), for a kind of account (`claude`, `chatgpt`): the first
    /// of [`Self::accounts`] that isn't at its usage limit (#252), or the first if they all are.
    pub async fn account(&self, session: &SessionContext, kind: &str) -> sqlx::Result<Account> {
        let accounts = self.accounts(session, kind).await?;
        let now = Utc::now();
        for account in &accounts {
            if !crate::quota::is_limited(&self.db, account.id, now).await? {
                return Ok(Account::Found(account.clone()));
            }
        }
        Ok(match accounts.into_iter().next() {
            Some(first) => Account::Found(first),
            None if self.named_accounts(session, kind).await?.is_some() => Account::Gone,
            None => Account::None,
        })
    }

    /// The accounts of a kind a session can use, in order: those its model grant names that are still there. A grant
    /// that names none, made before the owner had an account of the kind, uses the owner's default, and so do the
    /// runtime's judgements (ADR 26), whatever their session's grant says.
    pub async fn accounts(
        &self,
        session: &SessionContext,
        kind: &str,
    ) -> sqlx::Result<Vec<CredentialRow>> {
        let named = match session.role {
            SessionRole::Utility => None,
            _ => self.named_accounts(session, kind).await?,
        };
        Ok(match named {
            Some(accounts) => accounts,
            None => self
                .default_row(&session.owner, kind)
                .await?
                .into_iter()
                .collect(),
        })
    }

    /// The accounts of a kind a session's model grant names, in its order, leaving out those deleted since; `None`
    /// if it names none.
    pub async fn named_accounts(
        &self,
        session: &SessionContext,
        kind: &str,
    ) -> sqlx::Result<Option<Vec<CredentialRow>>> {
        let Some(ids) = session
            .grants
            .iter()
            .find(|g| crate::capabilities::account_kind(&g.capability) == Some(kind))
            .and_then(crate::grants::accounts)
        else {
            return Ok(None);
        };
        let ids: Vec<Uuid> = ids.iter().filter_map(|id| id.parse().ok()).collect();
        let rows: Vec<CredentialRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where owner = $1 and kind = $2 and id = any($3)"
        )))
        .bind(&session.owner)
        .bind(kind)
        .bind(&ids)
        .fetch_all(&self.db)
        .await?;
        Ok(Some(
            ids.iter()
                .filter_map(|id| rows.iter().find(|row| row.id == *id).cloned())
                .collect(),
        ))
    }

    /// One of the owner's credentials of a kind, by id.
    pub async fn owned(
        &self,
        owner: &str,
        kind: &str,
        id: &str,
    ) -> sqlx::Result<Option<CredentialRow>> {
        let Ok(id) = id.parse::<Uuid>() else {
            return Ok(None);
        };
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where id = $1 and owner = $2 and kind = $3"
        )))
        .bind(id)
        .bind(owner)
        .bind(kind)
        .fetch_optional(&self.db)
        .await
    }

    /// A credential's secret.
    pub fn reveal(&self, row: &CredentialRow) -> anyhow::Result<String> {
        self.vault.decrypt_string(&row.secret)
    }

    /// A ChatGPT login's access token and account id, refreshed first when it's about to expire. `None` if it was
    /// deleted meanwhile.
    pub async fn chatgpt(&self, login: &CredentialRow) -> anyhow::Result<Option<ChatGptTokens>> {
        let tokens: ChatGptTokens = serde_json::from_str(&self.reveal(login)?)?;
        if !tokens.expires_soon() {
            return Ok(Some(tokens));
        }
        let lock = self
            .chatgpt_refresh
            .lock()
            .await
            .entry(login.id)
            .or_default()
            .clone();
        let _refreshing = lock.lock().await;
        // Someone may have refreshed it while this waited for the lock.
        let row: Option<CredentialRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where id = $1"
        )))
        .bind(login.id)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let secret = self.vault.decrypt_string(&row.secret)?;
        let mut tokens: ChatGptTokens = serde_json::from_str(&secret)?;
        if !tokens.expires_soon() {
            return Ok(Some(tokens));
        }
        let response = self
            .http
            .post(CHATGPT_TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": CHATGPT_CLIENT_ID,
                "refresh_token": tokens.refresh_token,
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!(
                "refreshing the ChatGPT login failed ({}); sign in again with `codex login` and replace the credential",
                response.status()
            );
        }
        let refreshed: Value = response.json().await?;
        let field = |name: &str| refreshed[name].as_str().map(str::to_owned);
        if let Some(access) = field("access_token") {
            tokens.access_token = access;
        }
        if let Some(refresh) = field("refresh_token") {
            tokens.refresh_token = refresh;
        }
        if let Some(id) = field("id_token") {
            tokens.id_token = Some(id);
        }
        let sealed = self
            .vault
            .encrypt(serde_json::to_string(&tokens)?.as_bytes())?;
        // The old refresh token is spent now, so a login that isn't stored is lost until the user signs in again.
        if let Err(e) = sqlx::query("update credentials set secret = $2 where id = $1")
            .bind(row.id)
            .bind(&sealed)
            .execute(&self.db)
            .await
        {
            tracing::error!(owner = %row.owner, credential = %row.id, error = %e, "storing a refreshed ChatGPT login; it will need signing in again");
            return Err(e.into());
        }
        Ok(Some(tokens))
    }

    pub async fn github(&self, owner: &str) -> anyhow::Result<Arc<GitHubApp>> {
        let row = self
            .default_row(owner, GITHUB_APP)
            .await?
            .with_context(|| format!("{owner} has no GitHub App configured"))?;
        let mut cache = self.github.lock().await;
        if let Some(app) = cache.get(&row.id) {
            return Ok(app.clone());
        }
        let key = self.vault.decrypt(&row.secret)?;
        let app = Arc::new(GitHubApp::new(
            row.config("app_id").unwrap_or_default().to_owned(),
            &key,
        )?);
        cache.insert(row.id, app.clone());
        Ok(app)
    }

    /// The client to act with on a repository (ADR 29): the owner's GitHub App, or, when the session's grant names one,
    /// one of the owner's GitHub tokens. A credential that's gone, or isn't the owner's token, is an error, never the App.
    pub async fn github_as(
        &self,
        owner: &str,
        credential: Option<&str>,
    ) -> anyhow::Result<Arc<GitHubApp>> {
        let Some(credential) = credential else {
            return self.github(owner).await;
        };
        let row = self
            .github_user(owner, credential)
            .await?
            .with_context(|| {
                format!("the GitHub token {credential} this session acts with is gone")
            })?;
        let mut cache = self.github.lock().await;
        if let Some(client) = cache.get(&row.id) {
            return Ok(client.clone());
        }
        let token = self.vault.decrypt_string(&row.secret)?;
        let client = Arc::new(GitHubApp::for_user(
            token,
            row.config("login").unwrap_or_default().to_owned(),
        )?);
        cache.insert(row.id, client.clone());
        Ok(client)
    }

    /// One of the owner's `token` credentials, by id, with its secret.
    pub async fn token(
        &self,
        owner: &str,
        id: Uuid,
    ) -> anyhow::Result<Option<(CredentialRow, String)>> {
        let row: Option<CredentialRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from credentials where id = $1 and owner = $2 and kind = $3"
        )))
        .bind(id)
        .bind(owner)
        .bind(TOKEN)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let secret = self.vault.decrypt_string(&row.secret)?;
        Ok(Some((row, secret)))
    }

    /// The owner's GitHub tokens (`github-user`), oldest first.
    pub async fn github_users(&self, owner: &str) -> sqlx::Result<Vec<CredentialRow>> {
        Ok(self
            .list(owner)
            .await?
            .into_iter()
            .filter(|row| row.kind == GITHUB_USER)
            .collect())
    }

    /// One of the owner's GitHub tokens (`github-user`), by id.
    pub async fn github_user(&self, owner: &str, id: &str) -> sqlx::Result<Option<CredentialRow>> {
        self.owned(owner, GITHUB_USER, id).await
    }

    pub async fn memini(
        &self,
        owner: &str,
    ) -> anyhow::Result<Option<(MeminiAdmin, CredentialRow)>> {
        let Some((row, key)) = self.secret(owner, MEMINI).await? else {
            return Ok(None);
        };
        let admin = MeminiAdmin::new(row.config("url").unwrap_or_default(), key)?;
        Ok(Some((admin, row)))
    }

    pub async fn put_session_secret(
        &self,
        session: Uuid,
        name: &str,
        secret: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "insert into session_secrets (session_id, name, secret) values ($1, $2, $3) \
             on conflict (session_id, name) do update set secret = excluded.secret",
        )
        .bind(session)
        .bind(name)
        .bind(self.vault.encrypt(secret.as_bytes())?)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn session_secret(
        &self,
        session: Uuid,
        name: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "select secret from session_secrets where session_id = $1 and name = $2",
        )
        .bind(session)
        .bind(name)
        .fetch_optional(&self.db)
        .await?;
        row.map(|(sealed,)| self.vault.decrypt_string(&sealed))
            .transpose()
    }

    pub async fn delete_session_secret(&self, session: Uuid, name: &str) -> sqlx::Result<()> {
        sqlx::query("delete from session_secrets where session_id = $1 and name = $2")
            .bind(session)
            .bind(name)
            .execute(&self.db)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
impl Credentials {
    /// Stores `client` as one of the owner's GitHub credentials of `kind`, with `config`, without asking GitHub, the
    /// App as the default, and returns its id.
    pub(crate) async fn add_github_client(
        &self,
        owner: &str,
        kind: &str,
        config: Value,
        client: GitHubApp,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into credentials (id, owner, kind, name, config, secret, is_default) \
             values ($1, $2, $3, $4, $5, '', $6)",
        )
        .bind(id)
        .bind(owner)
        .bind(kind)
        .bind(id.to_string())
        .bind(config)
        .bind(kind == GITHUB_APP)
        .execute(&self.db)
        .await
        .unwrap();
        self.github.lock().await.insert(id, Arc::new(client));
        id
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use serde_json::json;
    use sloppod_contracts::Grant;

    use super::*;

    async fn credentials(db: PgPool) -> Credentials {
        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        Credentials::new(db, Vault::from_base64(&key).unwrap())
            .await
            .unwrap()
    }

    fn claude(name: &str) -> NewCredential {
        NewCredential {
            kind: CLAUDE.into(),
            name: name.into(),
            config: Default::default(),
            secret: "sk-test".into(),
        }
    }

    async fn add(credentials: &Credentials, owner: &str, new: NewCredential) -> CredentialRow {
        credentials.create(owner, new).await.unwrap().unwrap()
    }

    async fn session_with(db: &PgPool, owner: &str, grants: &[Grant]) -> Uuid {
        let id = Uuid::now_v7();
        crate::db::insert_session(
            db,
            crate::db::NewSessionRow {
                id,
                owner,
                title: "test",
                custom_title: false,
                pending_prompt: "",
                agent: "claude-code",
                kind: sloppod_protocol::api::SessionKind::Work,
                review: sloppod_protocol::api::ReviewPolicy::Off,
                model: None,
                repo: None,
                grants,
            },
        )
        .await
        .unwrap();
        id
    }

    async fn grant_of(db: &PgPool, session: Uuid, capability: &str) -> Grant {
        let row = crate::db::get_session(db, session).await.unwrap().unwrap();
        row.grants()
            .into_iter()
            .find(|g| g.capability == capability)
            .unwrap()
    }

    fn named(grant: &Grant) -> Vec<String> {
        crate::grants::accounts(grant)
            .unwrap_or_default()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[sqlx::test]
    async fn a_model_grant_names_every_account_default_first_and_keeps_them(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        add(&credentials, "someone", claude("Theirs")).await;
        credentials.set_default("dev", work.id).await.unwrap();
        let repo = json!({"repo": "a/b", "access": "ro"});
        let session = session_with(
            &db,
            "dev",
            &[
                Grant::new("model", json!({})),
                Grant::new("codex", json!({})),
                Grant::new("repo", repo.clone()),
            ],
        )
        .await;
        let ids = [work.id.to_string(), personal.id.to_string()];
        assert_eq!(named(&grant_of(&db, session, "model").await), ids);
        // No ChatGPT login: nothing to name. A repository names a credential only when the owner picks one.
        assert!(
            grant_of(&db, session, "codex")
                .await
                .params
                .get("credentials")
                .is_none()
        );
        assert_eq!(grant_of(&db, session, "repo").await.params, repo);

        // Another default changes what new grants get, not what this one uses.
        credentials.set_default("dev", personal.id).await.unwrap();
        assert_eq!(named(&grant_of(&db, session, "model").await), ids);
        let later = session_with(&db, "dev", &[]).await;
        crate::db::add_grant(&db, later, &Grant::new("model", json!({})))
            .await
            .unwrap();
        assert_eq!(
            named(&grant_of(&db, later, "model").await),
            [personal.id.to_string(), work.id.to_string()]
        );
        // A grant that names its accounts, like a sub-session's copy of its parent's, keeps them.
        let child = session_with(
            &db,
            "dev",
            &[Grant::new(
                "model",
                json!({"credentials": [work.id.to_string()]}),
            )],
        )
        .await;
        assert_eq!(
            named(&grant_of(&db, child, "model").await),
            [work.id.to_string()]
        );
    }

    #[sqlx::test]
    async fn a_session_uses_the_first_account_its_grant_names_that_is_still_there(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        let theirs = add(&credentials, "someone", claude("Theirs")).await;
        let context = |ids: &[Uuid], role: SessionRole| SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![Grant::new(
                "model",
                json!({"credentials": ids.iter().map(Uuid::to_string).collect::<Vec<_>>()}),
            )],
            role,
        };
        let used = async |session: &SessionContext| match credentials
            .account(session, CLAUDE)
            .await
            .unwrap()
        {
            Account::Found(row) => Some(row.name),
            Account::Gone => Some("gone".into()),
            Account::None => None,
        };
        let session = context(&[work.id, personal.id], SessionRole::Work);
        assert_eq!(used(&session).await.as_deref(), Some("Work"));
        // The runtime's judgements use the default, whatever their grant says.
        let utility = context(&[work.id], SessionRole::Utility);
        assert_eq!(used(&utility).await.as_deref(), Some("Personal"));
        // Another owner's account is never used.
        assert_eq!(
            used(&context(&[theirs.id], SessionRole::Work))
                .await
                .as_deref(),
            Some("gone")
        );

        // Usage is kept under the account used.
        let account = match credentials.account(&session, CLAUDE).await.unwrap() {
            Account::Found(row) => row,
            _ => unreachable!(),
        };
        let observation = crate::quota::Observation {
            observed_at: Utc::now(),
            limited: true,
            resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
            windows: Default::default(),
        };
        crate::quota::store(&db, account.id, &observation)
            .await
            .unwrap();
        let quotas = crate::quota::list(&db, "dev", Utc::now()).await.unwrap();
        assert_eq!(quotas.len(), 1);
        assert_eq!(
            (quotas[0].credential_id, quotas[0].limited),
            (work.id, true)
        );

        // Deleted, the next one is used; with none left, the grant is refused rather than given the default.
        credentials.delete("dev", work.id).await.unwrap();
        assert_eq!(used(&session).await.as_deref(), Some("Personal"));
        add(&credentials, "dev", claude("Another")).await;
        credentials.delete("dev", personal.id).await.unwrap();
        assert_eq!(used(&session).await.as_deref(), Some("gone"));
        // A grant made before the owner had an account uses the default.
        let unnamed = SessionContext {
            grants: vec![Grant::new("model", json!({}))],
            ..session.clone()
        };
        assert_eq!(used(&unnamed).await.as_deref(), Some("Another"));
    }

    #[sqlx::test]
    async fn a_session_moves_to_the_next_account_while_one_is_at_its_limit(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let session = SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![Grant::new(
                "model",
                json!({"credentials": [work.id.to_string(), personal.id.to_string()]}),
            )],
            role: SessionRole::Work,
        };
        let used = async || match credentials.account(&session, CLAUDE).await.unwrap() {
            Account::Found(row) => row.name,
            _ => unreachable!(),
        };
        let limit = async |id: Uuid, minutes: i64| {
            let observation = crate::quota::Observation {
                observed_at: Utc::now(),
                limited: true,
                resets_at: Some(Utc::now() + chrono::Duration::minutes(minutes)),
                windows: Default::default(),
            };
            crate::quota::store(&db, id, &observation).await.unwrap();
        };
        assert_eq!(used().await, "Work");
        limit(work.id, 60).await;
        assert_eq!(used().await, "Personal");
        // With every account limited, the first is used; requests on it are refused until a reset.
        limit(personal.id, 30).await;
        assert_eq!(used().await, "Work");
        // A limit whose reset has passed is over, and the first account is used again.
        let over = crate::quota::Observation {
            observed_at: Utc::now(),
            limited: true,
            resets_at: Some(Utc::now() - chrono::Duration::minutes(1)),
            windows: Default::default(),
        };
        crate::quota::store(&db, work.id, &over).await.unwrap();
        assert_eq!(used().await, "Work");
        assert!(
            !crate::quota::is_limited(&db, work.id, Utc::now())
                .await
                .unwrap()
        );
        assert!(
            crate::quota::is_limited(&db, personal.id, Utc::now())
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn a_limit_is_kept_against_the_account_that_was_used(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let session = SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![Grant::new(
                "model",
                json!({"credentials": [work.id.to_string(), personal.id.to_string()]}),
            )],
            role: SessionRole::Work,
        };
        let name = |row: Option<CredentialRow>| row.map(|r| r.name);
        let attached = match credentials.attach(&session, CLAUDE).await.unwrap() {
            Account::Found(row) => row.name,
            _ => unreachable!(),
        };
        assert_eq!(attached, "Work");
        // Work's report that it's limited comes before the turn fails: the account chosen now moves on, but the
        // failure is still Work's.
        let rejected = crate::quota::Observation {
            observed_at: Utc::now(),
            limited: true,
            resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
            windows: Default::default(),
        };
        crate::quota::store(&db, work.id, &rejected).await.unwrap();
        assert_eq!(
            name(credentials.used(&session, CLAUDE).await.unwrap()).as_deref(),
            Some("Work")
        );
        let (used, next) =
            crate::limits::fall_back(&credentials, &db, &session, CLAUDE, Utc::now())
                .await
                .unwrap();
        assert_eq!(
            (name(used).as_deref(), name(next).as_deref()),
            (Some("Work"), Some("Personal"))
        );
        assert!(
            !crate::quota::is_limited(&db, personal.id, Utc::now())
                .await
                .unwrap()
        );

        // Then Personal is attached, and hits its limit too: nothing is left to fall back to.
        credentials.attach(&session, CLAUDE).await.unwrap();
        let (used, next) =
            crate::limits::fall_back(&credentials, &db, &session, CLAUDE, Utc::now())
                .await
                .unwrap();
        assert_eq!(
            (name(used).as_deref(), name(next).as_deref()),
            (Some("Personal"), None)
        );
        assert!(
            crate::quota::is_limited(&db, personal.id, Utc::now())
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn a_limit_with_no_known_reset_is_tried_again_after_a_while(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let session = SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![Grant::new(
                "model",
                json!({"credentials": [work.id.to_string(), personal.id.to_string()]}),
            )],
            role: SessionRole::Work,
        };
        let used = async || match credentials.account(&session, CLAUDE).await.unwrap() {
            Account::Found(row) => row.name,
            _ => unreachable!(),
        };
        let reported = |minutes_ago: i64| crate::quota::Observation {
            observed_at: Utc::now() - chrono::Duration::minutes(minutes_ago),
            limited: true,
            resets_at: None,
            windows: Default::default(),
        };
        crate::quota::store(&db, work.id, &reported(10))
            .await
            .unwrap();
        assert_eq!(used().await, "Personal");
        sqlx::query("delete from credential_quotas")
            .execute(&db)
            .await
            .unwrap();
        crate::quota::store(&db, work.id, &reported(31))
            .await
            .unwrap();
        assert_eq!(used().await, "Work");
    }

    #[sqlx::test]
    async fn a_wait_ends_when_a_limit_with_no_known_reset_is_tried_again(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let session = SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![Grant::new(
                "model",
                json!({"credentials": [work.id.to_string(), personal.id.to_string()]}),
            )],
            role: SessionRole::Work,
        };
        let now = Utc::now();
        // Work said it's limited 20 minutes ago, without a reset; Personal is limited for five hours.
        let unknown = crate::quota::Observation {
            observed_at: now - chrono::Duration::minutes(20),
            limited: true,
            resets_at: None,
            windows: Default::default(),
        };
        crate::quota::store(&db, work.id, &unknown).await.unwrap();
        let known = crate::quota::Observation {
            observed_at: now,
            limited: true,
            resets_at: Some(now + chrono::Duration::hours(5)),
            windows: Default::default(),
        };
        crate::quota::store(&db, personal.id, &known).await.unwrap();
        let lasts = crate::limits::lasts(&credentials, &db, CLAUDE, &session, now).await;
        // Within the database's microseconds of Work's probe deadline, not Personal's reset.
        let Some(crate::limits::Lasts::Until(until)) = lasts else {
            panic!("{lasts:?}");
        };
        let deadline = unknown.observed_at + crate::quota::PROBE_AFTER;
        assert!(
            (until - deadline).num_milliseconds().abs() < 1,
            "{until} {deadline}"
        );
    }

    #[sqlx::test]
    async fn the_owner_gives_a_model_grant_only_their_own_accounts_of_its_kind(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let personal = add(&credentials, "dev", claude("Personal")).await;
        let work = add(&credentials, "dev", claude("Work")).await;
        let theirs = add(&credentials, "someone", claude("Theirs")).await;
        let model = Grant::new("model", json!({}));
        let given = async |grant: &Grant, ids: &[Uuid]| {
            let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
            crate::grants::accounts_given(&credentials, "dev", grant, &ids)
                .await
                .unwrap()
                .map(|rows| rows.into_iter().map(|r| r.name).collect::<Vec<_>>())
        };
        assert_eq!(
            given(&model, &[work.id, personal.id]).await.unwrap(),
            ["Work", "Personal"]
        );
        assert!(
            given(&model, &[])
                .await
                .unwrap_err()
                .contains("at least one")
        );
        assert!(given(&model, &[theirs.id]).await.is_err());
        assert!(
            given(&model, &[work.id, work.id])
                .await
                .unwrap_err()
                .contains("twice")
        );
        assert!(
            given(&Grant::new("codex", json!({})), &[work.id])
                .await
                .is_err()
        );
        assert!(
            given(&Grant::new("github", json!({})), &[work.id])
                .await
                .is_err()
        );
    }
    async fn defaults(credentials: &Credentials) -> Vec<String> {
        let rows = credentials.list("dev").await.unwrap();
        rows.into_iter()
            .filter(|r| r.is_default)
            .map(|r| r.name)
            .collect()
    }

    #[sqlx::test]
    async fn a_grant_s_github_token_is_used_as_it_is_and_never_swapped_for_the_app(db: PgPool) {
        let credentials = credentials(db.clone()).await;
        let id = Uuid::now_v7();
        let sealed = credentials.vault.encrypt(b"github_pat_x").unwrap();
        sqlx::query(
            "insert into credentials (id, owner, kind, name, config, secret, is_default) \
             values ($1, 'dev', $2, 'mine', '{\"login\": \"dev\"}', $3, true)",
        )
        .bind(id)
        .bind(GITHUB_USER)
        .bind(&sealed)
        .execute(&db)
        .await
        .unwrap();
        let id = id.to_string();

        // Without a GitHub App configured, as on a repository only the token reaches.
        let client = credentials.github_as("dev", Some(&id)).await.unwrap();
        assert_eq!(client.user_login(), Some("dev"));
        assert_eq!(
            client.token_for("dev/private").await.unwrap(),
            "github_pat_x"
        );
        assert!(
            credentials.github_as("dev", None).await.is_err(),
            "no App to fall back to"
        );
        assert!(
            credentials.github_as("other", Some(&id)).await.is_err(),
            "not another owner's"
        );
        assert!(
            credentials
                .github_as("dev", Some("nonsense"))
                .await
                .is_err()
        );
    }

    #[sqlx::test]
    async fn a_github_user_token_has_to_be_fine_grained(db: PgPool) {
        let credentials = credentials(db).await;
        for (secret, why) in [
            ("ghp_0123456789abcdef", "classic token"),
            ("gho_0123456789abcdef", "starts with github_pat_"),
        ] {
            let new = NewCredential {
                kind: GITHUB_USER.into(),
                name: "me".into(),
                config: Default::default(),
                secret: secret.into(),
            };
            let refused = credentials.create("dev", new).await.unwrap().unwrap_err();
            assert!(refused.0.contains(why), "{secret}: {}", refused.0);
        }
        assert!(credentials.list("dev").await.unwrap().is_empty());
        assert_eq!(user_token_shape("github_pat_11ABC"), Ok(()));
    }

    #[sqlx::test]
    async fn two_first_credentials_at_once_leave_one_default(db: PgPool) {
        let credentials = credentials(db).await;
        let (a, b) = tokio::join!(
            credentials.create("dev", claude("a")),
            credentials.create("dev", claude("b"))
        );
        assert!(a.unwrap().is_ok() && b.unwrap().is_ok());
        assert_eq!(defaults(&credentials).await.len(), 1);
    }

    #[sqlx::test]
    async fn deleting_the_default_while_another_is_chosen_leaves_one_default(db: PgPool) {
        let credentials = credentials(db).await;
        let x = credentials
            .create("dev", claude("x"))
            .await
            .unwrap()
            .unwrap();
        credentials
            .create("dev", claude("z"))
            .await
            .unwrap()
            .unwrap();
        let y = credentials
            .create("dev", claude("y"))
            .await
            .unwrap()
            .unwrap();
        let (deleted, chosen) = tokio::join!(
            credentials.delete("dev", x.id),
            credentials.set_default("dev", y.id)
        );
        assert!(deleted.unwrap());
        assert!(chosen.unwrap().is_some());
        assert_eq!(defaults(&credentials).await.len(), 1);

        // Deleting the default promotes one of the rest.
        let rows = credentials.list("dev").await.unwrap();
        let default = rows.iter().find(|r| r.is_default).unwrap();
        assert!(credentials.delete("dev", default.id).await.unwrap());
        assert_eq!(defaults(&credentials).await.len(), 1);
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn memini_urls_carry_no_credentials() {
        assert!(!has_userinfo("https://memini.example"));
        assert!(has_userinfo("https://admin@memini.example"));
        assert!(has_userinfo("https://admin:key@memini.example"));
    }
}
