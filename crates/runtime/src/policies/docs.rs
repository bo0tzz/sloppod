//! Library documentation through Context7's hosted MCP server (#195): agents look up current docs by library and
//! topic, and nothing else on the web opens up. Only its MCP endpoint is reachable.

use async_trait::async_trait;
use sloppod_contracts::capability;
use sloppod_contracts::{Decision, HttpRequest, Manifest, RequestPolicy, Result, SessionContext};

pub const HOST: &str = "mcp.context7.com";
/// Context7's MCP endpoint, as agents are given it.
pub const URL: &str = "https://mcp.context7.com/mcp";
const ENDPOINT: &str = "/mcp";

pub struct DocsPolicy;

#[async_trait]
impl RequestPolicy for DocsPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("docs-context7")
    }

    fn hosts(&self) -> Vec<String> {
        vec![HOST.into()]
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        if !session.has(capability::DOCS) {
            return Ok(Decision::deny(
                "sloppod: this session has no documentation access.\n",
            ));
        }
        if request.path_only() != ENDPOINT {
            return Ok(Decision::deny(format!(
                "sloppod: only Context7's MCP endpoint ({ENDPOINT}) is reachable; {} was refused.\n",
                request.path_only()
            )));
        }
        // Without an API key it's rate limited, which is enough for now.
        Ok(Decision::allow_with(Vec::new(), Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use sloppod_contracts::Grant;

    use super::*;

    fn request(path: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            host: HOST.into(),
            path: path.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            body_truncated: false,
        }
    }

    fn session(grants: Vec<Grant>) -> SessionContext {
        SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants,
            role: Default::default(),
        }
    }

    fn allowed(decision: Decision) -> bool {
        matches!(decision, Decision::Allow { .. })
    }

    #[tokio::test]
    async fn only_the_mcp_endpoint_with_the_grant() {
        let docs = session(vec![Grant::new(capability::DOCS, json!({}))]);
        assert!(allowed(
            DocsPolicy.decide(&docs, &request("/mcp")).await.unwrap()
        ));
        for path in ["/", "/api/v1/search", "/mcp/../api", "/mcpx"] {
            assert!(
                !allowed(DocsPolicy.decide(&docs, &request(path)).await.unwrap()),
                "{path}"
            );
        }
        let without = session(Vec::new());
        assert!(!allowed(
            DocsPolicy.decide(&without, &request("/mcp")).await.unwrap()
        ));
    }
}
