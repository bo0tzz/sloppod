mod codex;
mod github;
mod memini;
mod model;

use std::sync::Arc;

use sloppod_contracts::{RequestPolicy, SessionContext};

pub use codex::CodexPolicy;
pub use github::GitHubPolicy;
pub use memini::{MeminiPolicy, SESSION_KEY};
pub use model::ModelPolicy;

use crate::capabilities::{Part, holds};

/// The request policies the authorizer applies, each to the hosts it names, and the part each is the behaviour of
/// (ADR 2).
#[derive(Default, Clone)]
pub struct Policies {
    policies: Vec<(Part, Arc<dyn RequestPolicy>)>,
}

impl Policies {
    /// Registers `policy` as what a part of kind `serves` does: only a session holding one has its hosts sent to it.
    pub fn register(&mut self, serves: Part, policy: Arc<dyn RequestPolicy>) {
        self.policies.push((serves, policy));
    }

    pub fn for_host(&self, host: &str) -> Option<&Arc<dyn RequestPolicy>> {
        self.policies
            .iter()
            .map(|(_, p)| p)
            .find(|p| p.hosts().iter().any(|h| h.eq_ignore_ascii_case(host)))
    }

    /// Every policy's hosts, which the listeners' certificate covers.
    pub fn hosts(&self) -> Vec<String> {
        sorted(self.policies.iter().flat_map(|(_, p)| p.hosts()))
    }

    /// The hosts a session's egress sends through its listener: those of the policies behind parts it holds. It
    /// can't look up or reach the others at all.
    pub fn hosts_for(&self, session: &SessionContext) -> Vec<String> {
        sorted(
            self.policies
                .iter()
                .filter(|(serves, _)| holds(session, serves))
                .flat_map(|(_, p)| p.hosts()),
        )
    }
}

fn sorted(hosts: impl Iterator<Item = String>) -> Vec<String> {
    let mut hosts: Vec<String> = hosts.collect();
    hosts.sort();
    hosts.dedup();
    hosts
}
