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

fn any_with_ip(pods: &[std::sync::Arc<Pod>], ip: &str) -> bool {
    pods.iter()
        .any(|pod| pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()) == Some(ip))
}

fn sole_session(pods: &[std::sync::Arc<Pod>], ip: &str) -> Option<Uuid> {
    let mut sessions = pods
        .iter()
        .filter(|pod| live_with_ip(pod, ip))
        .filter_map(|pod| session_of(pod));
    let session = sessions.next()?;
    sessions.all(|other| other == session).then_some(session)
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
        sole_session(&self.store.state(), ip)
    }

    /// Whether any session pod, live or not, has or had `ip`. Wider than [`Self::session_for_ip`] on purpose: it's
    /// what keeps session pods off the user API, so an IP that's ambiguous between sessions still counts.
    pub fn is_session_ip(&self, ip: &str) -> bool {
        any_with_ip(&self.store.state(), ip)
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
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use k8s_openapi::api::core::v1::PodStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    use super::*;

    const IP: &str = "10.0.0.1";

    fn pod(session: Uuid, phase: &str, ip: &str) -> Arc<Pod> {
        Arc::new(Pod {
            metadata: ObjectMeta {
                labels: Some(BTreeMap::from([(
                    SESSION_LABEL.to_owned(),
                    session.to_string(),
                )])),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.into()),
                pod_ip: Some(ip.into()),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn deleting(pod: Arc<Pod>) -> Arc<Pod> {
        let mut pod = (*pod).clone();
        pod.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        Arc::new(pod)
    }

    #[test]
    fn an_ip_belongs_to_the_one_session_with_a_live_pod_on_it() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(sole_session(&[pod(a, "Running", IP)], IP), Some(a));
        assert_eq!(sole_session(&[pod(a, "Pending", IP)], IP), Some(a));
        assert_eq!(sole_session(&[pod(a, "Running", "10.0.0.2")], IP), None);
        // A finished or deleting pod that kept the IP doesn't count against the live one that has it now.
        assert_eq!(
            sole_session(&[pod(a, "Failed", IP), pod(b, "Running", IP)], IP),
            Some(b)
        );
        assert_eq!(
            sole_session(&[pod(a, "Succeeded", IP), pod(b, "Running", IP)], IP),
            Some(b)
        );
        assert_eq!(
            sole_session(
                &[deleting(pod(a, "Running", IP)), pod(b, "Running", IP)],
                IP
            ),
            Some(b)
        );
        // Two pods of one session are fine; two sessions on one IP match neither.
        assert_eq!(
            sole_session(&[pod(a, "Running", IP), pod(a, "Pending", IP)], IP),
            Some(a)
        );
        assert_eq!(
            sole_session(&[pod(a, "Running", IP), pod(b, "Running", IP)], IP),
            None
        );
    }

    #[test]
    fn any_session_pod_keeps_its_ip_off_the_user_api() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        // Ambiguous, finished or deleting: still a session's IP, so not a user's.
        assert!(any_with_ip(
            &[pod(a, "Running", IP), pod(b, "Running", IP)],
            IP
        ));
        assert!(any_with_ip(&[pod(a, "Failed", IP)], IP));
        assert!(any_with_ip(&[deleting(pod(a, "Running", IP))], IP));
        assert!(!any_with_ip(&[pod(a, "Running", "10.0.0.2")], IP));
    }
}
