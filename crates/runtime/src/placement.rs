//! Session pods as agent-sandbox `Sandbox`es (ADR 16).

use async_trait::async_trait;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
};
use serde_json::json;
use sloppod_contracts::{
    Error, Manifest, PlacementBackend, PlacementSpec, Result, Unschedulable, Volume,
};

pub const WORKDIR: &str = "/workspace";
pub const TOKEN_DIR: &str = "/var/run/secrets/sloppod";

pub fn sandbox_name(session_id: &str) -> String {
    format!("sloppod-{session_id}")
}

/// The session's workspace volume claim, as agent-sandbox names it from the Sandbox's `workdir` claim template.
fn claim_name(session_id: &str) -> String {
    format!("workdir-{}", sandbox_name(session_id))
}

/// The session a claim is the workspace volume of, by its name.
fn claim_session(name: &str) -> Option<&str> {
    name.strip_prefix("workdir-sloppod-")
        .filter(|rest| rest.parse::<uuid::Uuid>().is_ok())
}

/// A quantity kube gives, as bytes: none when the claim doesn't give it, or gives one that can't be read.
fn quantity(
    quantity: Option<&k8s_openapi::apimachinery::pkg::api::resource::Quantity>,
) -> Option<u64> {
    sloppod_contracts::storage_bytes(&quantity?.0)
}

/// A claim as the runtime sees it (#479): what it asks for, what's provisioned, and whether a resize is under way.
fn volume_of(claim: &k8s_openapi::api::core::v1::PersistentVolumeClaim) -> Option<Volume> {
    let name = claim.metadata.name.clone()?;
    let spec = claim.spec.as_ref();
    Some(Volume {
        session_id: claim_session(&name).map(str::to_owned),
        requested: quantity(
            spec.and_then(|spec| spec.resources.as_ref())
                .and_then(|resources| resources.requests.as_ref())
                .and_then(|requests| requests.get("storage")),
        ),
        capacity: quantity(
            claim
                .status
                .as_ref()
                .and_then(|status| status.capacity.as_ref())
                .and_then(|capacity| capacity.get("storage")),
        ),
        resizing: resizing(claim),
        name,
    })
}

/// Whether the claim says an expansion is on its way: the resizer still provisioning what it asked for, or a
/// filesystem resize waiting for the pod to be restarted to finish it (#479).
fn resizing(claim: &k8s_openapi::api::core::v1::PersistentVolumeClaim) -> bool {
    claim
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.status == "True"
                    && matches!(
                        condition.type_.as_str(),
                        "Resizing" | "FileSystemResizePending"
                    )
            })
        })
}

pub struct AgentSandbox {
    api: Api<DynamicObject>,
    pods: Api<k8s_openapi::api::core::v1::Pod>,
    claims: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim>,
    service_account: String,
    pull_policy: String,
    /// What's known of session pods, e.g. whether one can't be scheduled.
    index: crate::pods::PodIndex,
}

impl AgentSandbox {
    pub fn new(
        client: kube::Client,
        namespace: &str,
        service_account: String,
        pull_policy: String,
        index: crate::pods::PodIndex,
    ) -> Self {
        let gvk = GroupVersionKind::gvk("agents.x-k8s.io", "v1beta1", "Sandbox");
        let resource = ApiResource::from_gvk_with_plural(&gvk, "sandboxes");
        Self {
            api: Api::namespaced_with(client.clone(), namespace, &resource),
            pods: Api::namespaced(client.clone(), namespace),
            claims: Api::namespaced(client, namespace),
            service_account,
            pull_policy,
            index,
        }
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

/// The `Sandbox` a session is placed as: one pod with the session's workspace volume. With `spec.clone_from` set,
/// that volume starts as a copy of another session's claim — `dataSource` makes the cluster clone it, which only a
/// storage class that supports cloning can do (#478). Nothing here checks that: the session's supervisor is told the
/// workspace was copied, and the runtime falls back to a fresh one when the clone never arrives.
fn sandbox(spec: &PlacementSpec, service_account: &str, pull_policy: &str) -> serde_json::Value {
    let env: Vec<_> = spec
        .env
        .iter()
        .map(|(k, v)| json!({"name": k, "value": v}))
        .collect();
    let mut limits = serde_json::Map::new();
    let mut requests = serde_json::Map::new();
    if let Some(memory) = &spec.memory {
        limits.insert("memory".into(), json!(memory));
        requests.insert("memory".into(), json!(memory));
    }
    if let Some(cpu) = &spec.cpu {
        requests.insert("cpu".into(), json!(cpu));
    }
    let mut claim = json!({
        "accessModes": ["ReadWriteOnce"],
        "resources": {"requests": {"storage": spec.storage_size}},
    });
    if let Some(class) = &spec.storage_class {
        claim["storageClassName"] = json!(class);
    }
    if let Some(from) = &spec.clone_from {
        claim["dataSource"] = json!({"kind": "PersistentVolumeClaim", "name": claim_name(from)});
    }
    json!({
        "apiVersion": "agents.x-k8s.io/v1beta1",
        "kind": "Sandbox",
        "metadata": {"name": sandbox_name(&spec.session_id), "labels": spec.labels},
        "spec": {
            "operatingMode": "Running",
            "podTemplate": {
                "metadata": {"labels": spec.labels},
                "spec": {
                    "serviceAccountName": service_account,
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "terminationGracePeriodSeconds": 90,
                    "securityContext": {
                        "runAsUser": 1000,
                        "runAsGroup": 1000,
                        "fsGroup": 1000,
                        // Re-own the volume only when its root isn't already the fsGroup, not on every mount:
                        // a workdir with many files (node_modules, git objects) is slow to chown each time,
                        // so a remounted session pod would otherwise stall while the kubelet walks it.
                        "fsGroupChangePolicy": "OnRootMismatch",
                        "runAsNonRoot": true,
                        "seccompProfile": {"type": "RuntimeDefault"},
                    },
                    "containers": [{
                        "name": "session",
                        "image": spec.image,
                        "imagePullPolicy": pull_policy,
                        "env": env,
                        "resources": {"limits": limits, "requests": requests},
                        // The supervisor keeps the agent through link drops; a link down past its grace
                        // period (or a wedged supervisor) restarts the container (ADR 17).
                        "livenessProbe": {
                            "exec": {"command": ["sloppod-supervisor", "--check"]},
                            "periodSeconds": 30,
                            "failureThreshold": 3,
                        },
                        "volumeMounts": [
                            {"name": "workdir", "mountPath": WORKDIR},
                            {"name": "sloppod-token", "mountPath": TOKEN_DIR, "readOnly": true},
                        ],
                        "securityContext": {
                            "allowPrivilegeEscalation": false,
                            "capabilities": {"drop": ["ALL"]},
                        },
                    }],
                    "volumes": [{
                        "name": "sloppod-token",
                        "projected": {"sources": [{"serviceAccountToken": {
                            "audience": "sloppod",
                            "expirationSeconds": 3600,
                            "path": "token",
                        }}]},
                    }],
                },
            },
            "volumeClaimTemplates": [{"metadata": {"name": "workdir"}, "spec": claim}],
        },
    })
}

#[async_trait]
impl PlacementBackend for AgentSandbox {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("placement-agent-sandbox")
    }

    async fn ensure(&self, spec: &PlacementSpec) -> Result<()> {
        let name = sandbox_name(&spec.session_id);
        let sandbox = sandbox(spec, &self.service_account, &self.pull_policy);
        self.api
            .patch(
                &name,
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(&sandbox),
            )
            .await
            .map_err(err)?;
        Ok(())
    }

    /// Whether a session's volume is there to copy (#478): its claim exists and the cluster has provisioned it.
    async fn can_copy_workspace(&self, from: &str) -> Result<bool> {
        Ok(self
            .volume(from)
            .await?
            .is_some_and(|volume| volume.capacity.is_some()))
    }

    async fn suspend(&self, session_id: &str) -> Result<()> {
        let patch = json!({"spec": {"operatingMode": "Suspended"}});
        match self
            .api
            .patch(
                &sandbox_name(session_id),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(err(e)),
        }
    }

    async fn replace_pod(&self, session_id: &str) -> Result<()> {
        // The Sandbox stays as it is, so its controller starts a new pod from the current spec in its place.
        // Deleting gracefully gives the supervisor its shutdown: the report and the transcript sync.
        let selector = format!("{}={session_id}", crate::pods::SESSION_LABEL);
        self.pods
            .delete_collection(
                &DeleteParams::default(),
                &kube::api::ListParams::default().labels(&selector),
            )
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<()> {
        let gone = |result: kube::Result<_>| match result {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(err(e)),
        };
        // The Sandbox owns its pod and volume claim, so they go with it; the claim is deleted by name as well, for
        // one left without its Sandbox (#455).
        gone(
            self.api
                .delete(&sandbox_name(session_id), &DeleteParams::foreground())
                .await
                .map(|_| ()),
        )?;
        gone(
            self.claims
                .delete(&claim_name(session_id), &DeleteParams::default())
                .await
                .map(|_| ()),
        )
    }

    async fn sessions(&self) -> Result<Vec<String>> {
        let selector = kube::api::ListParams::default().labels(crate::pods::SESSION_LABEL);
        let mut sessions: Vec<String> = self
            .api
            .list(&selector)
            .await
            .map_err(err)?
            .items
            .iter()
            .filter_map(|sandbox| {
                sandbox
                    .metadata
                    .labels
                    .as_ref()?
                    .get(crate::pods::SESSION_LABEL)
                    .cloned()
            })
            .collect();
        let claims = self
            .claims
            .list(&kube::api::ListParams::default())
            .await
            .map_err(err)?;
        sessions.extend(
            claims.items.iter().filter_map(|claim| {
                claim_session(claim.metadata.name.as_deref()?).map(str::to_owned)
            }),
        );
        sessions.sort();
        sessions.dedup();
        Ok(sessions)
    }

    async fn volumes(&self) -> Result<Vec<Volume>> {
        let claims = self
            .claims
            .list(&kube::api::ListParams::default())
            .await
            .map_err(err)?;
        Ok(claims
            .items
            .iter()
            // Only sessions' workspace claims: the namespace may hold others.
            .filter_map(volume_of)
            .filter(|volume| volume.session_id.is_some())
            .collect())
    }

    async fn volume(&self, session_id: &str) -> Result<Option<Volume>> {
        let claim = self
            .claims
            .get_opt(&claim_name(session_id))
            .await
            .map_err(err)?;
        Ok(claim.as_ref().and_then(volume_of))
    }

    async fn grow_volume(&self, session_id: &str, size: u64) -> Result<()> {
        // A merge patch, so the claim's other requests survive it. The storage class has to allow expansion, or the
        // cluster refuses it here.
        let patch = json!({"spec": {"resources": {"requests": {"storage": size.to_string()}}}});
        self.claims
            .patch(
                &claim_name(session_id),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
            .map(|_| ())
            .map_err(err)
    }

    fn unschedulable(&self) -> Vec<Unschedulable> {
        self.index.unschedulable()
    }

    async fn pod_gone(&self, session_id: &str) -> Result<bool> {
        let selector = format!("{}={session_id}", crate::pods::SESSION_LABEL);
        let pods = self
            .pods
            .list(&kube::api::ListParams::default().labels(&selector))
            .await
            .map_err(err)?;
        Ok(pods.items.is_empty())
    }

    fn has_pod(&self, session_id: &str) -> bool {
        session_id
            .parse()
            .is_ok_and(|session| self.index.has_pod(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_a_claim_is_and_what_it_asks_for() {
        let claim: k8s_openapi::api::core::v1::PersistentVolumeClaim =
            serde_json::from_value(json!({
                "metadata": {"name": "workdir-sloppod-0194f0a2-0000-7000-8000-000000000000"},
                "spec": {"resources": {"requests": {"storage": "30Gi"}}},
                "status": {"capacity": {"storage": "20Gi"}},
            }))
            .unwrap();
        let volume = volume_of(&claim).unwrap();
        assert_eq!(
            volume.session_id.as_deref(),
            Some("0194f0a2-0000-7000-8000-000000000000")
        );
        assert_eq!(volume.requested, Some(30 << 30));
        assert_eq!(volume.capacity, Some(20 << 30));
        assert!(volume.growing());
        assert_eq!(claim_session("data-postgres-0"), None);

        // Waiting for its pod to be restarted before the filesystem can catch up.
        let claim: k8s_openapi::api::core::v1::PersistentVolumeClaim =
            serde_json::from_value(json!({
                "metadata": {"name": "workdir-sloppod-0194f0a2-0000-7000-8000-000000000000"},
                "spec": {"resources": {"requests": {"storage": "30Gi"}}},
                "status": {
                    "capacity": {"storage": "30Gi"},
                    "conditions": [{"type": "FileSystemResizePending", "status": "True"}],
                },
            }))
            .unwrap();
        let volume = volume_of(&claim).unwrap();
        assert_eq!(volume.size(), Some(30 << 30));
        assert!(volume.growing());

        // A claim of no session, e.g. another workload's in the same namespace.
        let other: k8s_openapi::api::core::v1::PersistentVolumeClaim =
            serde_json::from_value(json!({"metadata": {"name": "data-postgres-0"}})).unwrap();
        assert_eq!(volume_of(&other).unwrap().session_id, None);
    }
}
