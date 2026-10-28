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

/// The session's live pod: the image it runs, and when it was made.
fn session_pod(pods: &[std::sync::Arc<Pod>], session: Uuid) -> Option<SessionPod> {
    pods.iter()
        .filter(|pod| session_of(pod) == Some(session) && is_live(pod))
        .find_map(|pod| {
            let image = pod
                .spec
                .as_ref()?
                .containers
                .iter()
                .find(|c| c.name == "session")?
                .image
                .clone()?;
            let made = pod
                .metadata
                .creation_timestamp
                .as_ref()
                .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0));
            Some(SessionPod { image, made })
        })
}

/// What's known of a session's live pod.
pub struct SessionPod {
    pub image: String,
    pub made: Option<chrono::DateTime<chrono::Utc>>,
}

fn is_live(pod: &Pod) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && !matches!(
            pod.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        )
}

fn live_with_ip(pod: &Pod, ip: &str) -> bool {
    is_live(pod) && pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()) == Some(ip)
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
    /// The image the session's live pod runs, if it has one.
    pub fn session_image(&self, session: Uuid) -> Option<String> {
        self.session_pod(session).map(|pod| pod.image)
    }

    /// The session's live pod, if it has one.
    pub fn session_pod(&self, session: Uuid) -> Option<SessionPod> {
        session_pod(&self.store.state(), session)
    }

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

    fn with_image(pod: Arc<Pod>, image: &str) -> Arc<Pod> {
        let mut pod = (*pod).clone();
        pod.spec = Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![k8s_openapi::api::core::v1::Container {
                name: "session".into(),
                image: Some(image.into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        Arc::new(pod)
    }

    #[test]
    fn a_session_runs_its_live_pods_image() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let old = with_image(deleting(pod(a, "Running", IP)), "session:old");
        let new = with_image(pod(a, "Running", IP), "session:new");
        let image = |pods: &[Arc<Pod>], session| session_pod(pods, session).map(|pod| pod.image);
        assert_eq!(
            image(&[old.clone(), new.clone()], a).as_deref(),
            Some("session:new")
        );
        assert_eq!(image(&[old], a), None);
        assert_eq!(image(&[with_image(pod(b, "Running", IP), "x")], a), None);
        // And when it was made.
        let mut made = (*new).clone();
        made.metadata.creation_timestamp = Some(Time(
            k8s_openapi::jiff::Timestamp::from_second(1_800_000_000).unwrap(),
        ));
        assert_eq!(
            session_pod(&[Arc::new(made)], a)
                .and_then(|pod| pod.made)
                .map(|t| t.timestamp()),
            Some(1_800_000_000)
        );
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
