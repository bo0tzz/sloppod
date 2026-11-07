//! The LiteLLM operator's proxies as a model provider (#461, ADR 7): the runtime watches `LiteLLMProxy` and
//! `LiteLLMModel` resources in the namespaces it's configured for, offers each proxy's models to every user, and gives
//! each session on one a `LiteLLMVirtualKey` of its own, limited to the session's model, whose key the operator mints
//! into a Secret the runtime reads and attaches. The pod never sees it, and it goes with the session.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
};
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Grant, Manifest, ModelProvider, ModelTarget, OfferedModel, Result, SessionContext,
};

use crate::endpoint::EndpointModel;

const GROUP: &str = "litellm.home-operations.com";
const VERSION: &str = "v1alpha1";
/// How a proxy's location starts: `litellm/<namespace>/<proxy>`.
const LOCATION: &str = "litellm/";
/// The port a proxy's Service serves on, unless its spec says otherwise.
const DEFAULT_PORT: i64 = 4000;
/// How long a request waits for the operator to mint a session's key before it's refused.
const KEY_WAIT: Duration = Duration::from_secs(15);

fn resource(kind: &str, plural: &str) -> ApiResource {
    ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(GROUP, VERSION, kind), plural)
}

/// A proxy's location in a model's id.
fn location(namespace: &str, name: &str) -> String {
    format!("{LOCATION}{namespace}/{name}")
}

/// The namespace and name a location names, if it's a proxy's.
fn proxy_of(location: &str) -> Option<(&str, &str)> {
    let (namespace, name) = location.strip_prefix(LOCATION)?.split_once('/')?;
    (!namespace.is_empty() && !name.is_empty() && !name.contains('/')).then_some((namespace, name))
}

fn namespace_of(object: &DynamicObject) -> &str {
    object.metadata.namespace.as_deref().unwrap_or_default()
}

fn name_of(object: &DynamicObject) -> &str {
    object.metadata.name.as_deref().unwrap_or_default()
}

/// Whether a label selector (`matchLabels`, `matchExpressions`) matches `labels`.
fn selects(selector: &Value, labels: &BTreeMap<String, String>) -> bool {
    let match_labels = selector["matchLabels"].as_object().into_iter().flatten();
    let labels_match = match_labels
        .clone()
        .all(|(key, value)| labels.get(key).map(String::as_str) == value.as_str());
    let expressions_match = selector["matchExpressions"]
        .as_array()
        .into_iter()
        .flatten()
        .all(|expression| {
            let key = expression["key"].as_str().unwrap_or_default();
            let values: Vec<&str> = expression["values"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let value = labels.get(key).map(String::as_str);
            match expression["operator"].as_str() {
                Some("In") => value.is_some_and(|v| values.contains(&v)),
                Some("NotIn") => !value.is_some_and(|v| values.contains(&v)),
                Some("Exists") => value.is_some(),
                Some("DoesNotExist") => value.is_none(),
                _ => false,
            }
        });
    labels_match && expressions_match
}

/// Whether a model is bound to a proxy, as the operator binds them, most specific first: the model's `proxyRef` (with
/// its `proxyNamespace`, or its own namespace); otherwise, in the proxy's namespace, its `modelSelector`; otherwise
/// every model in the proxy's namespace.
pub fn bound(proxy: &DynamicObject, model: &DynamicObject) -> bool {
    let spec = &model.data["spec"];
    if let Some(proxy_ref) = spec["proxyRef"].as_str().filter(|r| !r.is_empty()) {
        let namespace = spec["proxyNamespace"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or(namespace_of(model));
        return proxy_ref == name_of(proxy) && namespace == namespace_of(proxy);
    }
    if namespace_of(model) != namespace_of(proxy) {
        return false;
    }
    match &proxy.data["spec"]["modelSelector"] {
        Value::Null => true,
        selector => selects(
            selector,
            model.metadata.labels.as_ref().unwrap_or(&BTreeMap::new()),
        ),
    }
}

/// The models the proxies offer, every proxy's bound models, named by their `modelName`, with their `info`.
pub fn offered(proxies: &[Arc<DynamicObject>], models: &[Arc<DynamicObject>]) -> Vec<OfferedModel> {
    let mut offered = Vec::new();
    for proxy in proxies {
        let at = location(namespace_of(proxy), name_of(proxy));
        for model in models.iter().filter(|model| bound(proxy, model)) {
            let Some(name) = model.data["spec"]["modelName"]
                .as_str()
                .filter(|n| !n.is_empty())
            else {
                continue;
            };
            let value = EndpointModel {
                location: at.clone(),
                model: name.to_owned(),
            }
            .id();
            if offered.iter().any(|m: &OfferedModel| m.value == value) {
                continue;
            }
            offered.push(OfferedModel {
                value,
                name: format!("{name} (LiteLLM {})", name_of(proxy)),
                model: name.to_owned(),
                info: model.data["spec"]["info"].clone(),
            });
        }
    }
    offered
}

/// The base URL of a proxy's Service.
pub fn service_url(proxy: &DynamicObject, cluster_domain: &str) -> String {
    let port = proxy.data["spec"]["service"]["port"]
        .as_i64()
        .unwrap_or(DEFAULT_PORT);
    format!(
        "http://{}.{}.svc.{cluster_domain}:{port}",
        name_of(proxy),
        namespace_of(proxy)
    )
}

/// A session's virtual key's name, and its Secret's.
fn key_name(session_id: &str) -> String {
    format!("sloppod-{session_id}")
}

fn secret_name(session_id: &str) -> String {
    format!("sloppod-{session_id}-key")
}

/// The `LiteLLMVirtualKey` a session on `model` of the proxy at `namespace/proxy` gets: limited to that model, named
/// for the session and its owner, its key in a Secret of its own.
pub fn virtual_key(session: &SessionContext, namespace: &str, proxy: &str, model: &str) -> Value {
    json!({
        "apiVersion": format!("{GROUP}/{VERSION}"),
        "kind": "LiteLLMVirtualKey",
        "metadata": {
            "name": key_name(&session.session_id),
            "labels": {
                crate::pods::SESSION_LABEL: session.session_id,
                "app.kubernetes.io/part-of": "sloppod",
            },
        },
        "spec": {
            "proxyRef": proxy,
            "proxyNamespace": namespace,
            "secretName": secret_name(&session.session_id),
            "models": [model],
            "keyAlias": key_name(&session.session_id),
            "metadata": {"sloppod_session": session.session_id, "sloppod_owner": session.owner},
        },
    })
}

/// What the runtime does with sessions' virtual keys in the cluster.
#[async_trait]
pub trait Keys: Send + Sync {
    /// Creates or updates a virtual key, by server-side apply.
    async fn apply(&self, key: &Value) -> anyhow::Result<()>;
    /// The key the operator minted into a Secret, if it has yet.
    async fn secret(&self, name: &str) -> anyhow::Result<Option<String>>;
    /// Deletes a virtual key; the operator deletes the remote key and its Secret with it.
    async fn delete(&self, name: &str) -> anyhow::Result<()>;
}

/// [`Keys`] in the runtime's namespace, through the Kubernetes API.
pub struct ClusterKeys {
    keys: Api<DynamicObject>,
    secrets: Api<Secret>,
}

impl ClusterKeys {
    pub fn new(client: &kube::Client, namespace: &str) -> Self {
        Self {
            keys: Api::namespaced_with(
                client.clone(),
                namespace,
                &resource("LiteLLMVirtualKey", "litellmvirtualkeys"),
            ),
            secrets: Api::namespaced(client.clone(), namespace),
        }
    }
}

#[async_trait]
impl Keys for ClusterKeys {
    async fn apply(&self, key: &Value) -> anyhow::Result<()> {
        let name = key["metadata"]["name"].as_str().unwrap_or_default();
        self.keys
            .patch(
                name,
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(key),
            )
            .await?;
        Ok(())
    }

    async fn secret(&self, name: &str) -> anyhow::Result<Option<String>> {
        let Some(secret) = self.secrets.get_opt(name).await? else {
            return Ok(None);
        };
        Ok(secret
            .data
            .and_then(|data| data.get("key").cloned())
            .and_then(|key| String::from_utf8(key.0).ok()))
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        match self.keys.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// The proxies and models the runtime watches, in each namespace it's configured for.
#[derive(Clone)]
pub struct Catalog {
    proxies: Vec<Store<DynamicObject>>,
    models: Vec<Store<DynamicObject>>,
}

fn watch(api: Api<DynamicObject>, kind: &ApiResource, what: &'static str) -> Store<DynamicObject> {
    let writer = reflector::store::Writer::new(kind.clone());
    let store = writer.as_reader();
    let stream = reflector::reflector(writer, watcher(api, watcher::Config::default()))
        .default_backoff()
        .applied_objects();
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        while let Some(event) = stream.next().await {
            if let Err(e) = event {
                tracing::warn!(error = %e, what, "LiteLLM operator watch");
            }
        }
    });
    store
}

impl Catalog {
    /// Watches the operator's proxies and models in `namespaces`.
    pub fn watch(client: &kube::Client, namespaces: &[String]) -> Self {
        let (proxy, model) = (
            resource("LiteLLMProxy", "litellmproxies"),
            resource("LiteLLMModel", "litellmmodels"),
        );
        let api = |namespace: &str, kind: &ApiResource| {
            Api::namespaced_with(client.clone(), namespace, kind)
        };
        Self {
            proxies: namespaces
                .iter()
                .map(|ns| watch(api(ns, &proxy), &proxy, "proxies"))
                .collect(),
            models: namespaces
                .iter()
                .map(|ns| watch(api(ns, &model), &model, "models"))
                .collect(),
        }
    }

    /// One over stores filled some other way, for tests.
    #[cfg(test)]
    pub fn of(proxies: Store<DynamicObject>, models: Store<DynamicObject>) -> Self {
        Self {
            proxies: vec![proxies],
            models: vec![models],
        }
    }

    fn proxies(&self) -> Vec<Arc<DynamicObject>> {
        self.proxies.iter().flat_map(|s| s.state()).collect()
    }

    fn models(&self) -> Vec<Arc<DynamicObject>> {
        self.models.iter().flat_map(|s| s.state()).collect()
    }

    fn proxy(&self, namespace: &str, name: &str) -> Option<Arc<DynamicObject>> {
        self.proxies()
            .into_iter()
            .find(|p| namespace_of(p) == namespace && name_of(p) == name)
    }
}

/// The LiteLLM operator's proxies, as a model provider.
pub struct LiteLlm {
    catalog: Catalog,
    keys: Arc<dyn Keys>,
    cluster_domain: String,
    /// Sessions' keys as read from their Secrets: read once, not on every request.
    minted: Mutex<HashMap<String, String>>,
    /// How long a request waits for the operator to mint a session's key.
    key_wait: Duration,
}

impl LiteLlm {
    pub fn new(catalog: Catalog, keys: Arc<dyn Keys>, cluster_domain: &str) -> Self {
        Self {
            catalog,
            keys,
            cluster_domain: cluster_domain.to_owned(),
            minted: Mutex::default(),
            key_wait: KEY_WAIT,
        }
    }

    fn proxy_of_grant(&self, grant: &Grant) -> Option<Arc<DynamicObject>> {
        let (namespace, name) = proxy_of(grant.param("location")?)?;
        self.catalog.proxy(namespace, name)
    }
}

fn contract_error(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

#[async_trait]
impl ModelProvider for LiteLlm {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("endpoint-litellm-operator")
    }

    fn owns(&self, location: &str) -> bool {
        proxy_of(location).is_some()
    }

    /// The same for every user: the proxies are the deployment's.
    async fn models(&self, _owner: &str) -> Result<Vec<OfferedModel>> {
        Ok(offered(&self.catalog.proxies(), &self.catalog.models()))
    }

    async fn grant(
        &self,
        _owner: &str,
        location: &str,
        model: &str,
    ) -> Result<std::result::Result<Value, String>> {
        let wanted = EndpointModel {
            location: location.to_owned(),
            model: model.to_owned(),
        }
        .id();
        if !offered(&self.catalog.proxies(), &self.catalog.models())
            .iter()
            .any(|m| m.value == wanted)
        {
            return Ok(Err(format!(
                "no LiteLLM proxy at {location} serves {model} any more"
            )));
        }
        Ok(Ok(json!({"model": model})))
    }

    async fn provision(&self, session: &SessionContext, grant: &Grant) -> Result<()> {
        let (Some((namespace, proxy)), Some(model)) = (
            grant.param("location").and_then(proxy_of),
            grant.param("model"),
        ) else {
            return Err(Error::new(
                "the session's endpoint grant names no LiteLLM proxy and model",
            ));
        };
        self.keys
            .apply(&virtual_key(session, namespace, proxy, model))
            .await
            .map_err(contract_error)
    }

    async fn target(&self, session: &SessionContext, grant: &Grant) -> Result<Option<ModelTarget>> {
        let Some(proxy) = self.proxy_of_grant(grant) else {
            return Ok(None);
        };
        let base_url = service_url(&proxy, &self.cluster_domain);
        let cached = self
            .minted
            .lock()
            .unwrap()
            .get(&session.session_id)
            .cloned();
        if let Some(key) = cached {
            return Ok(Some(ModelTarget { base_url, key }));
        }
        // The operator mints the key a moment after the virtual key is made; a session's first request may beat it.
        let secret = secret_name(&session.session_id);
        let deadline = tokio::time::Instant::now() + self.key_wait;
        loop {
            if let Some(key) = self.keys.secret(&secret).await.map_err(contract_error)? {
                self.minted
                    .lock()
                    .unwrap()
                    .insert(session.session_id.clone(), key.clone());
                return Ok(Some(ModelTarget { base_url, key }));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn release(&self, session_id: &str, _grant: &Grant) -> Result<()> {
        self.minted.lock().unwrap().remove(session_id);
        self.keys
            .delete(&key_name(session_id))
            .await
            .map_err(contract_error)
    }
}

#[cfg(test)]
mod tests {
    use kube::runtime::watcher::Event;

    use super::*;

    fn object(
        kind: &str,
        namespace: &str,
        name: &str,
        labels: Value,
        spec: Value,
    ) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": format!("{GROUP}/{VERSION}"),
            "kind": kind,
            "metadata": {"name": name, "namespace": namespace, "labels": labels},
            "spec": spec,
        }))
        .unwrap()
    }

    fn store(objects: Vec<DynamicObject>) -> Store<DynamicObject> {
        let mut writer = reflector::store::Writer::new(resource("LiteLLMModel", "litellmmodels"));
        let store = writer.as_reader();
        writer.apply_watcher_event(&Event::Init);
        for object in objects {
            writer.apply_watcher_event(&Event::InitApply(object));
        }
        writer.apply_watcher_event(&Event::InitDone);
        store
    }

    /// A `main` proxy in `ai` that adopts its namespace's models, a `picky` one that selects `tier: cheap`, and
    /// models bound every way the operator binds them.
    fn catalog() -> Catalog {
        let proxy = |name: &str, spec: Value| object("LiteLLMProxy", "ai", name, json!({}), spec);
        let model = |namespace: &str, name: &str, labels: Value, spec: Value| {
            object("LiteLLMModel", namespace, name, labels, spec)
        };
        Catalog::of(
            store(vec![
                proxy("main", json!({})),
                proxy(
                    "picky",
                    json!({"modelSelector": {"matchLabels": {"tier": "cheap"}}, "service": {"port": 8000}}),
                ),
            ]),
            store(vec![
                model(
                    "ai",
                    "glm",
                    json!({}),
                    json!({"modelName": "glm-5.2", "params": {"model": "openai/glm-5.2"}, "info": {"maxInputTokens": 1000000, "supportsFunctionCalling": true}}),
                ),
                model(
                    "ai",
                    "qwen",
                    json!({"tier": "cheap"}),
                    json!({"modelName": "qwen3-coder", "params": {"model": "hosted_vllm/qwen3"}}),
                ),
                // Named for the picky proxy, so not adopted by main.
                model(
                    "ai",
                    "pinned",
                    json!({}),
                    json!({"modelName": "pinned", "proxyRef": "picky", "params": {"model": "x"}}),
                ),
                // Elsewhere, but naming main across namespaces.
                model(
                    "apps",
                    "remote",
                    json!({}),
                    json!({"modelName": "remote", "proxyRef": "main", "proxyNamespace": "ai", "params": {"model": "y"}}),
                ),
                // Elsewhere, naming nothing: no proxy here adopts it.
                model(
                    "apps",
                    "stray",
                    json!({}),
                    json!({"modelName": "stray", "params": {"model": "z"}}),
                ),
            ]),
        )
    }

    #[test]
    fn a_proxy_offers_the_models_bound_to_it() {
        let catalog = catalog();
        let offered = offered(&catalog.proxies(), &catalog.models());
        let mut got: Vec<(String, String)> = offered
            .iter()
            .map(|m| (m.value.clone(), m.name.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                (
                    "endpoint:litellm/ai/main:glm-5.2".to_owned(),
                    "glm-5.2 (LiteLLM main)".to_owned()
                ),
                (
                    "endpoint:litellm/ai/main:qwen3-coder".to_owned(),
                    "qwen3-coder (LiteLLM main)".to_owned()
                ),
                (
                    "endpoint:litellm/ai/main:remote".to_owned(),
                    "remote (LiteLLM main)".to_owned()
                ),
                (
                    "endpoint:litellm/ai/picky:pinned".to_owned(),
                    "pinned (LiteLLM picky)".to_owned()
                ),
                (
                    "endpoint:litellm/ai/picky:qwen3-coder".to_owned(),
                    "qwen3-coder (LiteLLM picky)".to_owned()
                ),
            ]
        );
        let glm = offered.iter().find(|m| m.model == "glm-5.2").unwrap();
        assert_eq!(glm.info["maxInputTokens"], 1000000);
        let picky = catalog.proxy("ai", "picky").unwrap();
        assert_eq!(
            service_url(&picky, "cluster.local"),
            "http://picky.ai.svc.cluster.local:8000"
        );
        assert_eq!(proxy_of("litellm/ai/main"), Some(("ai", "main")));
        assert_eq!(proxy_of("0195f3a2-0000-7000-8000-000000000000"), None);
    }

    /// Sessions' virtual keys as a cluster would keep them, with the keys the operator would mint.
    #[derive(Default)]
    struct FakeKeys {
        applied: Mutex<Vec<Value>>,
        secrets: Mutex<HashMap<String, String>>,
        deleted: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Keys for FakeKeys {
        async fn apply(&self, key: &Value) -> anyhow::Result<()> {
            self.applied.lock().unwrap().push(key.clone());
            Ok(())
        }

        async fn secret(&self, name: &str) -> anyhow::Result<Option<String>> {
            Ok(self.secrets.lock().unwrap().get(name).cloned())
        }

        async fn delete(&self, name: &str) -> anyhow::Result<()> {
            self.deleted.lock().unwrap().push(name.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_session_on_a_proxy_s_model_gets_a_key_of_its_own_that_goes_with_it() {
        use sloppod_contracts::RequestPolicy;

        let keys = Arc::new(FakeKeys::default());
        let mut litellm = LiteLlm::new(catalog(), keys.clone(), "cluster.local");
        litellm.key_wait = Duration::from_millis(50);
        let mut providers = crate::endpoint::Providers::default();
        providers.register(Arc::new(litellm));
        let providers = Arc::new(providers);

        let model = EndpointModel::parse("endpoint:litellm/ai/main:glm-5.2").unwrap();
        assert_eq!(
            providers.resolve("dev", "glm-5.2").await,
            Ok(Some(model.clone()))
        );
        let grant = providers.grant("dev", &model).await.unwrap().unwrap();
        assert_eq!(
            grant.params,
            json!({"location": "litellm/ai/main", "model": "glm-5.2"})
        );
        let stray = EndpointModel::parse("endpoint:litellm/apps/nothing:stray").unwrap();
        assert!(providers.grant("dev", &stray).await.unwrap().is_err());

        let session = SessionContext {
            session_id: "0195f3a2".into(),
            owner: "dev".into(),
            grants: vec![grant.clone()],
            role: Default::default(),
        };
        // Placing the session makes its virtual key: limited to its model, naming it and its owner.
        providers.provision(&session).await.unwrap();
        let applied = keys.applied.lock().unwrap().clone();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0]["metadata"]["name"], "sloppod-0195f3a2");
        assert_eq!(
            applied[0]["spec"],
            json!({
                "proxyRef": "main", "proxyNamespace": "ai", "secretName": "sloppod-0195f3a2-key",
                "models": ["glm-5.2"], "keyAlias": "sloppod-0195f3a2",
                "metadata": {"sloppod_session": "0195f3a2", "sloppod_owner": "dev"},
            })
        );

        // Until the operator has minted it, requests are refused; then they carry it, to the proxy's Service.
        let policy = crate::policies::EndpointPolicy::new(providers.clone());
        let request = sloppod_contracts::HttpRequest {
            method: "POST".into(),
            host: String::new(),
            path: "/v1/messages".into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            body_truncated: false,
        };
        assert!(matches!(
            policy.decide(&session, &request).await.unwrap(),
            sloppod_contracts::Decision::Deny { .. }
        ));
        keys.secrets
            .lock()
            .unwrap()
            .insert("sloppod-0195f3a2-key".into(), "sk-minted".into());
        assert_eq!(
            policy.decide(&session, &request).await.unwrap(),
            sloppod_contracts::Decision::allow_with(
                vec![
                    ("authorization".into(), "Bearer sk-minted".into()),
                    ("x-api-key".into(), "sk-minted".into())
                ],
                Vec::new()
            )
        );
        assert_eq!(
            providers.target(&session).await.unwrap().unwrap().base_url,
            "http://main.ai.svc.cluster.local:4000"
        );

        // The session goes, and its key with it.
        providers
            .release(&session.session_id, &session.grants)
            .await;
        assert_eq!(*keys.deleted.lock().unwrap(), ["sloppod-0195f3a2"]);
    }
}
