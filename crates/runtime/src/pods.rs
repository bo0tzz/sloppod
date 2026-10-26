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

    pub fn session_for_ip(&self, ip: &str) -> Option<Uuid> {
        self.store
            .state()
            .iter()
            .find(|pod| {
                pod.metadata.deletion_timestamp.is_none()
                    && pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()) == Some(ip)
            })
            .and_then(|pod| session_of(pod))
    }

    pub fn session_for_pod(&self, name: &str) -> Option<Uuid> {
        self.store
            .state()
            .iter()
            .find(|pod| pod.metadata.name.as_deref() == Some(name))
            .and_then(|pod| session_of(pod))
    }
}
