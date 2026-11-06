//! Session pods as agent-sandbox `Sandbox`es (ADR 16).

use async_trait::async_trait;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
};
use serde_json::json;
use sloppod_contracts::{Error, Manifest, PlacementBackend, PlacementSpec, Result, Unschedulable};

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

#[async_trait]
impl PlacementBackend for AgentSandbox {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("placement-agent-sandbox")
    }

    async fn ensure(&self, spec: &PlacementSpec) -> Result<()> {
        let name = sandbox_name(&spec.session_id);
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
        let sandbox = json!({
            "apiVersion": "agents.x-k8s.io/v1beta1",
            "kind": "Sandbox",
            "metadata": {"name": name, "labels": spec.labels},
            "spec": {
                "operatingMode": "Running",
                "podTemplate": {
                    "metadata": {"labels": spec.labels},
                    "spec": {
                        "serviceAccountName": self.service_account,
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
                            "imagePullPolicy": self.pull_policy,
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
        });
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

    fn unschedulable(&self) -> Vec<Unschedulable> {
        self.index.unschedulable()
    }

    fn has_pod(&self, session_id: &str) -> bool {
        session_id
            .parse()
            .is_ok_and(|session| self.index.has_pod(session))
    }
}
