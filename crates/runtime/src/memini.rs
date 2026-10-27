//! memini administration with the admin key: one key per session (ADR 22).

use std::time::Duration;

use anyhow::bail;
use serde_json::{Value, json};

pub struct MeminiAdmin {
    http: reqwest::Client,
    base_url: String,
    admin_key: String,
}

pub struct SessionKey {
    pub name: String,
    pub secret: String,
}

impl MeminiAdmin {
    pub fn new(base_url: &str, admin_key: String) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("sloppod")
                .timeout(Duration::from_secs(30))
                .build()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            admin_key,
        })
    }

    pub fn key_name(session_id: &str) -> String {
        format!("sloppod-{session_id}")
    }

    /// A key bound to the owner's home, defaulting to the session's namespace, read-only unless it may write.
    pub async fn create_session_key(
        &self,
        session_id: &str,
        home: Option<&str>,
        namespace: &str,
        read_only: bool,
    ) -> anyhow::Result<SessionKey> {
        let name = Self::key_name(session_id);
        let mut body =
            json!({"name": name, "default_namespace": namespace, "read_only": read_only});
        if let Some(home) = home {
            body["home"] = json!(home);
        }
        let response = self
            .http
            .post(format!("{}/v1/keys", self.base_url))
            .bearer_auth(&self.admin_key)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "memini refused the session key ({status}): {}",
                body["error"].as_str().unwrap_or("no message")
            );
        }
        let Some(secret) = body["secret"].as_str() else {
            bail!("memini returned no secret for the session key");
        };
        Ok(SessionKey {
            name,
            secret: secret.to_owned(),
        })
    }

    pub async fn delete_key(&self, name: &str) -> anyhow::Result<()> {
        let response = self
            .http
            .delete(format!("{}/v1/keys/{name}", self.base_url))
            .bearer_auth(&self.admin_key)
            .send()
            .await?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            bail!(
                "memini refused to delete key {name} ({})",
                response.status()
            );
        }
        Ok(())
    }
}
