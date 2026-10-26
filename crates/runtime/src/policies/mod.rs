mod github;
mod memini;
mod model;

use std::collections::HashMap;
use std::sync::Arc;

use sloppod_contracts::RequestPolicy;

pub use github::GitHubPolicy;
pub use memini::MeminiPolicy;
pub use model::ModelPolicy;

/// Request policies by the host they decide for.
#[derive(Default, Clone)]
pub struct Policies {
    by_host: HashMap<String, Arc<dyn RequestPolicy>>,
}

impl Policies {
    pub fn register(&mut self, policy: Arc<dyn RequestPolicy>) {
        for host in policy.hosts() {
            self.by_host
                .insert(host.to_ascii_lowercase(), policy.clone());
        }
    }

    pub fn for_host(&self, host: &str) -> Option<&Arc<dyn RequestPolicy>> {
        self.by_host.get(&host.to_ascii_lowercase())
    }

    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.by_host.keys().cloned().collect();
        hosts.sort();
        hosts
    }
}
