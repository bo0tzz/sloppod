//! Which pod belongs to which session, from a watch on session pods.

use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::Api;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use uuid::Uuid;

pub const SESSION_LABEL: &str = "sloppod.dev/session";

#[derive(Clone)]
pub struct PodIndex {
    store: Store<Pod>,
}

fn session_of(pod: &Pod) -> Option<Uuid> {
    pod.metadata
        .labels
        .as_ref()?
        .get(SESSION_LABEL)?
        .parse()
        .ok()
}

fn live_with_ip(pod: &Pod, ip: &str) -> bool {
    let Some(status) = &pod.status else {
        return false;
    };
    pod.metadata.deletion_timestamp.is_none()
        && !matches!(status.phase.as_deref(), Some("Succeeded" | "Failed"))
        && status.pod_ip.as_deref() == Some(ip)
}

impl PodIndex {
    pub fn start(client: kube::Client, namespace: &str) -> Self {
        let api: Api<Pod> = Api::namespaced(client, namespace);
        let (store, writer) = reflector::store();
        let stream = reflector::reflector(
            writer,
            watcher(api, watcher::Config::default().labels(SESSION_LABEL)),
        )
        .default_backoff()
        .applied_objects();
        tokio::spawn(async move {
            futures::pin_mut!(stream);
            while let Some(event) = stream.next().await {
                if let Err(e) = event {
                    tracing::warn!(error = %e, "session pod watch");
                }
            }
        });
        Self { store }
    }

    /// The session whose live pod has `ip`. A pod that has finished (evicted, say) keeps its IP in its status while
    /// the IP goes to another pod, so only live ones count; if two still claim it, neither gets it.
    pub fn session_for_ip(&self, ip: &str) -> Option<Uuid> {
        let pods = self.store.state();
        let mut sessions = pods
            .iter()
            .filter(|pod| live_with_ip(pod, ip))
            .filter_map(|pod| session_of(pod));
        let session = sessions.next()?;
        sessions.all(|other| other == session).then_some(session)
    }

    pub fn session_for_pod(&self, name: &str) -> Option<Uuid> {
        self.store
            .state()
            .iter()
            .find(|pod| pod.metadata.name.as_deref() == Some(name))
            .and_then(|pod| session_of(pod))
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::PodStatus;

    use super::*;

    fn pod(phase: &str, ip: &str) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some(phase.into()),
                pod_ip: Some(ip.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_live_pods_hold_their_ip() {
        assert!(live_with_ip(&pod("Running", "10.0.0.1"), "10.0.0.1"));
        assert!(live_with_ip(&pod("Pending", "10.0.0.1"), "10.0.0.1"));
        assert!(!live_with_ip(&pod("Failed", "10.0.0.1"), "10.0.0.1"));
        assert!(!live_with_ip(&pod("Succeeded", "10.0.0.1"), "10.0.0.1"));
        assert!(!live_with_ip(&pod("Running", "10.0.0.2"), "10.0.0.1"));
    }
}
