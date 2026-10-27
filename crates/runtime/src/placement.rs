//! Session pods as agent-sandbox `Sandbox`es (ADR 16).

use async_trait::async_trait;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
};
use serde_json::json;
use sloppod_contracts::{Error, Manifest, PlacementBackend, PlacementSpec, Result};

pub const WORKDIR: &str = "/workspace";
pub const TOKEN_DIR: &str = "/var/run/secrets/sloppod";

pub fn sandbox_name(session_id: &str) -> String {
    format!("sloppod-{session_id}")
}

pub struct AgentSandbox {
    api: Api<DynamicObject>,
    service_account: String,
    pull_policy: String,
}

impl AgentSandbox {
    pub fn new(
        client: kube::Client,
        namespace: &str,
        service_account: String,
        pull_policy: String,
    ) -> Self {
        let gvk = GroupVersionKind::gvk("agents.x-k8s.io", "v1beta1", "Sandbox");
        let resource = ApiResource::from_gvk_with_plural(&gvk, "sandboxes");
        Self {
            api: Api::namespaced_with(client, namespace, &resource),
            service_account,
            pull_policy,
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

    async fn delete(&self, session_id: &str) -> Result<()> {
        match self
            .api
            .delete(&sandbox_name(session_id), &DeleteParams::foreground())
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(err(e)),
        }
    }
}
