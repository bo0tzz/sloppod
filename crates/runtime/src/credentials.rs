//! Credentials the runtime uses on a user's behalf, stored encrypted (ADR 21).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;
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
const KINDS: &[&str] = &[CLAUDE, GITHUB_APP, MEMINI, CHATGPT];

/// Codex's OAuth client and token endpoint, which refresh a ChatGPT login.
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// A ChatGPT login's tokens, as stored. Refresh tokens are single-use, so the runtime is their only holder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatGptTokens {
    pub access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    pub account_id: String,
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

/// A credential request that can't be stored, with a reason for the user.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Invalid(pub String);

const COLUMNS: &str = "id, owner, kind, name, config, secret, is_default, created_at";

pub struct Credentials {
    db: PgPool,
    vault: Vault,
    github: Mutex<HashMap<Uuid, Arc<GitHubApp>>>,
    /// Held while refreshing a ChatGPT login, so a single-use refresh token is only ever spent once.
    chatgpt_refresh: Mutex<()>,
    http: reqwest::Client,
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

impl Credentials {
    pub async fn new(db: PgPool, vault: Vault) -> anyhow::Result<Self> {
        Ok(Self {
            db,
            vault,
            github: Mutex::default(),
            chatgpt_refresh: Mutex::default(),
            http: reqwest::Client::new(),
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
            _ => serde_json::json!({}),
        };
        let sealed = self.vault.encrypt(secret.as_bytes())?;
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
        .fetch_optional(&self.db)
        .await?;
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
        sqlx::query("update credentials set is_default = (id = $1) where owner = $2 and kind = $3")
            .bind(id)
            .bind(owner)
            .bind(&row.kind)
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
        let deleted: Option<(String, bool)> = sqlx::query_as(
            "delete from credentials where id = $1 and owner = $2 returning kind, is_default",
        )
        .bind(id)
        .bind(owner)
        .fetch_optional(&self.db)
        .await?;
        let Some((kind, was_default)) = deleted else {
            return Ok(false);
        };
        if was_default {
            sqlx::query(
                "update credentials set is_default = true where id = \
                 (select id from credentials where owner = $1 and kind = $2 order by created_at limit 1)",
            )
            .bind(owner)
            .bind(&kind)
            .execute(&self.db)
            .await?;
        }
        self.github.lock().await.remove(&id);
        Ok(true)
    }

    /// Whether the owner has a credential of a kind, without decrypting it.
    pub async fn has_default(&self, owner: &str, kind: &str) -> sqlx::Result<bool> {
        Ok(self.default_row(owner, kind).await?.is_some())
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

    /// The owner's ChatGPT access token and account id, refreshed first when it's about to expire.
    pub async fn chatgpt(&self, owner: &str) -> anyhow::Result<Option<ChatGptTokens>> {
        let Some((_, secret)) = self.secret(owner, CHATGPT).await? else {
            return Ok(None);
        };
        let tokens: ChatGptTokens = serde_json::from_str(&secret)?;
        if !tokens.expires_soon() {
            return Ok(Some(tokens));
        }
        let _refreshing = self.chatgpt_refresh.lock().await;
        // Someone may have refreshed it while this waited for the lock.
        let Some((row, secret)) = self.secret(owner, CHATGPT).await? else {
            return Ok(None);
        };
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
        sqlx::query("update credentials set secret = $2 where id = $1")
            .bind(row.id)
            .bind(&sealed)
            .execute(&self.db)
            .await?;
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
