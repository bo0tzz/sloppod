mod codex;
mod github;
mod memini;
mod model;

use std::sync::Arc;

use sloppod_contracts::RequestPolicy;

pub use codex::CodexPolicy;
pub use github::GitHubPolicy;
pub use memini::{MeminiPolicy, SESSION_KEY};
pub use model::ModelPolicy;

/// Request policies. Their hosts can change at runtime, e.g. when a memini credential is added.
#[derive(Default, Clone)]
pub struct Policies {
    policies: Vec<Arc<dyn RequestPolicy>>,
}

impl Policies {
    pub fn register(&mut self, policy: Arc<dyn RequestPolicy>) {
        self.policies.push(policy);
    }

    pub fn for_host(&self, host: &str) -> Option<&Arc<dyn RequestPolicy>> {
        self.policies
            .iter()
            .find(|p| p.hosts().iter().any(|h| h.eq_ignore_ascii_case(host)))
    }

    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.policies.iter().flat_map(|p| p.hosts()).collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }
}
