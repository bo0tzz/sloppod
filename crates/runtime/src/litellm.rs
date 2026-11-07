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

/// A session's virtual key's name for the proxy at `location`, and its Secret's: one per session and proxy, so a
/// session moving to another proxy gets a key of its own there while its old one is deleted with its own proxy
/// reference. A Kubernetes name can't hold the location itself, so it's a hash of it.
fn key_name(session_id: &str, location: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(location.as_bytes());
    let short: String = hash[..5].iter().map(|b| format!("{b:02x}")).collect();
    format!("sloppod-{session_id}-{short}")
}

fn secret_name(session_id: &str, location: &str) -> String {
    format!("{}-key", key_name(session_id, location))
}

/// A minted key's identity for the spend kept per key: its virtual key's name, and a digest of the key itself, since a
/// session that comes back to a proxy gets a new key under the same name, with spend of its own.
fn spend_id(name: &str, key: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(key.as_bytes());
    let short: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("litellm:{name}:{short}")
}

/// The label every session virtual key carries, which the sweep lists them by.
const KEY_LABEL: &str = "sloppod.dev/endpoint-key";

/// The `LiteLLMVirtualKey` a session on `model` of the proxy at `namespace/proxy` gets: limited to that model, named
/// for the session and its owner, its key in a Secret of its own.
pub fn virtual_key(
    session: &SessionContext,
    namespace: &str,
    proxy: &str,
    model: &str,
    budget: Option<f64>,
) -> Value {
    let at = location(namespace, proxy);
    let mut key = json!({
        "apiVersion": format!("{GROUP}/{VERSION}"),
        "kind": "LiteLLMVirtualKey",
        "metadata": {
            "name": key_name(&session.session_id, &at),
            "labels": {
                crate::pods::SESSION_LABEL: session.session_id,
                KEY_LABEL: "litellm",
                "app.kubernetes.io/part-of": "sloppod",
            },
        },
        "spec": {
            "proxyRef": proxy,
            "proxyNamespace": namespace,
            "secretName": secret_name(&session.session_id, &at),
            "models": [model],
            "keyAlias": key_name(&session.session_id, &at),
            "metadata": {"sloppod_session": session.session_id, "sloppod_owner": session.owner},
        },
    });
    // The owner's budget per session, which LiteLLM enforces on the key.
    if let Some(budget) = budget {
        key["spec"]["maxBudget"] = json!(budget.to_string());
    }
    key
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
    /// The sessions' virtual keys there are: each one's name, and when it was made.
    async fn list(&self) -> anyhow::Result<Vec<(String, Option<chrono::DateTime<chrono::Utc>>)>>;
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

    async fn list(&self) -> anyhow::Result<Vec<(String, Option<chrono::DateTime<chrono::Utc>>)>> {
        let keys = self
            .keys
            .list(&kube::api::ListParams::default().labels(KEY_LABEL))
            .await?;
        Ok(keys
            .items
            .into_iter()
            .filter_map(|k| {
                let made = k
                    .metadata
                    .creation_timestamp
                    .and_then(|t| chrono::DateTime::from_timestamp(t.0.as_second(), 0));
                Some((k.metadata.name?, made))
            })
            .collect())
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
            .apply(&virtual_key(
                session,
                namespace,
                proxy,
                model,
                grant.params["max_budget"].as_f64(),
            ))
            .await
            .map_err(contract_error)
    }

    async fn target(&self, session: &SessionContext, grant: &Grant) -> Result<Option<ModelTarget>> {
        let (Some(proxy), Some(at)) = (self.proxy_of_grant(grant), grant.param("location")) else {
            return Ok(None);
        };
        let base_url = service_url(&proxy, &self.cluster_domain);
        let name = key_name(&session.session_id, at);
        let cached = self.minted.lock().unwrap().get(&name).cloned();
        if let Some(key) = cached {
            return Ok(Some(ModelTarget {
                base_url,
                key_id: spend_id(&name, &key),
                key,
                shared: None,
            }));
        }
        // The operator mints the key a moment after the virtual key is made; a session's first request may beat it.
        let secret = secret_name(&session.session_id, at);
        let deadline = tokio::time::Instant::now() + self.key_wait;
        loop {
            if let Some(key) = self.keys.secret(&secret).await.map_err(contract_error)? {
                self.minted
                    .lock()
                    .unwrap()
                    .insert(name.clone(), key.clone());
                return Ok(Some(ModelTarget {
                    base_url,
                    key_id: spend_id(&name, &key),
                    key,
                    shared: None,
                }));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn release(&self, session_id: &str, grant: &Grant) -> Result<()> {
        let Some(at) = grant.param("location") else {
            return Ok(());
        };
        let name = key_name(session_id, at);
        self.minted.lock().unwrap().remove(&name);
        self.keys.delete(&name).await.map_err(contract_error)
    }

    /// Deletes the virtual keys of sessions that don't hold their grant any more: a release that failed, a session
    /// purged while the runtime was down.
    async fn sweep(&self, keep: &[(String, Grant)]) -> Result<()> {
        let wanted: Vec<String> = keep
            .iter()
            .filter_map(|(session, grant)| Some(key_name(session, grant.param("location")?)))
            .collect();
        // A key made a moment ago may be for a grant being recorded now.
        let settled = chrono::Utc::now() - chrono::Duration::minutes(10);
        for (name, made) in self.keys.list().await.map_err(contract_error)? {
            if !wanted.contains(&name) && made.is_none_or(|made| made < settled) {
                self.minted.lock().unwrap().remove(&name);
                self.keys.delete(&name).await.map_err(contract_error)?;
                tracing::info!(key = %name, "deleted a virtual key no session holds");
            }
        }
        Ok(())
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

    type Made = Option<chrono::DateTime<chrono::Utc>>;

    /// Sessions' virtual keys as a cluster would keep them (by name, with when each was made), with the keys the
    /// operator would mint.
    #[derive(Default)]
    struct FakeKeys {
        keys: Mutex<BTreeMap<String, (Value, Made)>>,
        secrets: Mutex<HashMap<String, String>>,
        fail_apply: std::sync::atomic::AtomicBool,
    }

    impl FakeKeys {
        fn spec(&self, name: &str) -> Option<Value> {
            self.keys
                .lock()
                .unwrap()
                .get(name)
                .map(|(key, _)| key["spec"].clone())
        }

        fn names(&self) -> Vec<String> {
            self.keys.lock().unwrap().keys().cloned().collect()
        }
    }

    #[async_trait]
    impl Keys for FakeKeys {
        async fn apply(&self, key: &Value) -> anyhow::Result<()> {
            if self.fail_apply.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("the API server refused it");
            }
            let name = key["metadata"]["name"].as_str().unwrap().to_owned();
            let mut keys = self.keys.lock().unwrap();
            let made = keys
                .get(&name)
                .map_or(Some(chrono::Utc::now()), |(_, made)| *made);
            keys.insert(name, (key.clone(), made));
            Ok(())
        }

        async fn secret(&self, name: &str) -> anyhow::Result<Option<String>> {
            Ok(self.secrets.lock().unwrap().get(name).cloned())
        }

        async fn delete(&self, name: &str) -> anyhow::Result<()> {
            self.keys.lock().unwrap().remove(name);
            Ok(())
        }

        async fn list(
            &self,
        ) -> anyhow::Result<Vec<(String, Option<chrono::DateTime<chrono::Utc>>)>> {
            Ok(self
                .keys
                .lock()
                .unwrap()
                .iter()
                .map(|(name, (_, made))| (name.clone(), *made))
                .collect())
        }
    }

    fn providers_with(keys: &Arc<FakeKeys>) -> Arc<crate::endpoint::Providers> {
        let mut litellm = LiteLlm::new(catalog(), keys.clone(), "cluster.local");
        litellm.key_wait = Duration::from_millis(50);
        let mut providers = crate::endpoint::Providers::default();
        providers.register(Arc::new(litellm));
        Arc::new(providers)
    }

    fn on(grant: Option<&Grant>) -> SessionContext {
        SessionContext {
            session_id: "0195f3a2".into(),
            owner: "dev".into(),
            grants: grant.into_iter().cloned().collect(),
            role: Default::default(),
        }
    }

    async fn grant_of(providers: &crate::endpoint::Providers, model: &str) -> Grant {
        let model = EndpointModel::parse(model).unwrap();
        providers.grant("dev", &model).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_session_on_a_proxy_s_model_gets_a_key_of_its_own_that_goes_with_it() {
        use sloppod_contracts::RequestPolicy;

        let keys = Arc::new(FakeKeys::default());
        let providers = providers_with(&keys);
        let model = EndpointModel::parse("endpoint:litellm/ai/main:glm-5.2").unwrap();
        assert_eq!(
            providers.resolve("dev", "glm-5.2").await,
            Ok(Some(model.clone()))
        );
        let grant = grant_of(&providers, &model.id()).await;
        assert_eq!(
            grant.params,
            json!({"location": "litellm/ai/main", "model": "glm-5.2"})
        );
        let stray = EndpointModel::parse("endpoint:litellm/apps/nothing:stray").unwrap();
        assert!(providers.grant("dev", &stray).await.unwrap().is_err());

        let session = on(Some(&grant));
        // Placing the session makes its virtual key: limited to its model, naming it and its owner.
        providers.provision(&session).await.unwrap();
        let name = key_name("0195f3a2", "litellm/ai/main");
        assert!(name.starts_with("sloppod-0195f3a2-"));
        assert_eq!(keys.names(), std::slice::from_ref(&name));
        assert_eq!(
            keys.spec(&name).unwrap(),
            json!({
                "proxyRef": "main", "proxyNamespace": "ai", "secretName": format!("{name}-key"),
                "models": ["glm-5.2"], "keyAlias": name,
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
            .insert(format!("{name}-key"), "sk-minted".into());
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
            .await
            .unwrap();
        assert!(keys.names().is_empty());
    }

    #[tokio::test]
    async fn a_move_to_another_proxy_gets_a_key_there_and_deletes_only_the_old_one() {
        let keys = Arc::new(FakeKeys::default());
        let providers = providers_with(&keys);
        let main = grant_of(&providers, "endpoint:litellm/ai/main:glm-5.2").await;
        let picky = grant_of(&providers, "endpoint:litellm/ai/picky:qwen3-coder").await;
        providers.provision(&on(Some(&main))).await.unwrap();
        // The move provisions the new one beside the old, which keeps its own proxy reference.
        providers.provision(&on(Some(&picky))).await.unwrap();
        let (old, new) = (
            key_name("0195f3a2", "litellm/ai/main"),
            key_name("0195f3a2", "litellm/ai/picky"),
        );
        assert_ne!(old, new);
        assert_eq!(keys.spec(&old).unwrap()["proxyRef"], "main");
        assert_eq!(keys.spec(&new).unwrap()["proxyRef"], "picky");
        keys.secrets
            .lock()
            .unwrap()
            .insert(format!("{old}-key"), "sk-main".into());
        keys.secrets
            .lock()
            .unwrap()
            .insert(format!("{new}-key"), "sk-picky".into());
        assert_eq!(
            providers
                .target(&on(Some(&main)))
                .await
                .unwrap()
                .unwrap()
                .key,
            "sk-main"
        );
        // Once it's moved, the old one goes, and the new one serves its requests.
        providers
            .release("0195f3a2", std::slice::from_ref(&main))
            .await
            .unwrap();
        assert_eq!(keys.names(), std::slice::from_ref(&new));
        let target = providers.target(&on(Some(&picky))).await.unwrap().unwrap();
        assert_eq!(
            (target.key.as_str(), target.base_url.as_str()),
            ("sk-picky", "http://picky.ai.svc.cluster.local:8000")
        );
    }

    #[tokio::test]
    async fn a_move_put_back_puts_its_keys_back_too() {
        // On the same proxy: the key is limited to its old model again.
        let keys = Arc::new(FakeKeys::default());
        let providers = providers_with(&keys);
        let glm = grant_of(&providers, "endpoint:litellm/ai/main:glm-5.2").await;
        let qwen = grant_of(&providers, "endpoint:litellm/ai/main:qwen3-coder").await;
        providers.provision(&on(Some(&glm))).await.unwrap();
        providers.provision(&on(Some(&qwen))).await.unwrap();
        let name = key_name("0195f3a2", "litellm/ai/main");
        assert_eq!(keys.spec(&name).unwrap()["models"], json!(["qwen3-coder"]));
        crate::endpoint::undo_provisioning(&providers, &on(Some(&glm)), Some(&glm), Some(&qwen))
            .await
            .unwrap();
        assert_eq!(keys.names(), std::slice::from_ref(&name));
        assert_eq!(keys.spec(&name).unwrap()["models"], json!(["glm-5.2"]));

        // From a model of the agent's own (or another endpoint): the key made for the move goes.
        let keys = Arc::new(FakeKeys::default());
        let providers = providers_with(&keys);
        let glm = grant_of(&providers, "endpoint:litellm/ai/main:glm-5.2").await;
        providers.provision(&on(Some(&glm))).await.unwrap();
        crate::endpoint::undo_provisioning(&providers, &on(None), None, Some(&glm))
            .await
            .unwrap();
        assert!(keys.names().is_empty());

        // A key the API refuses fails the move, which the switch then puts back.
        keys.fail_apply
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(providers.provision(&on(Some(&glm))).await.is_err());
    }

    #[tokio::test]
    async fn the_sweep_deletes_keys_no_session_holds() {
        let keys = Arc::new(FakeKeys::default());
        let providers = providers_with(&keys);
        let glm = grant_of(&providers, "endpoint:litellm/ai/main:glm-5.2").await;
        providers.provision(&on(Some(&glm))).await.unwrap();
        // Left behind by a release that failed, long ago, and one made a moment ago for a grant being recorded.
        let stale = key_name("old-session", "litellm/ai/main");
        let fresh = key_name("new-session", "litellm/ai/main");
        keys.keys.lock().unwrap().insert(
            stale.clone(),
            (
                json!({}),
                Some(chrono::Utc::now() - chrono::Duration::hours(2)),
            ),
        );
        keys.keys
            .lock()
            .unwrap()
            .insert(fresh.clone(), (json!({}), Some(chrono::Utc::now())));
        providers.sweep(&[("0195f3a2".to_owned(), glm)]).await;
        let mut left = keys.names();
        left.sort();
        let mut expected = vec![key_name("0195f3a2", "litellm/ai/main"), fresh];
        expected.sort();
        assert_eq!(left, expected);
    }

    #[tokio::test]
    async fn a_key_made_again_for_a_proxy_has_a_spend_of_its_own() {
        let keys = Arc::new(FakeKeys::default());
        let providers = providers_with(&keys);
        let main = grant_of(&providers, "endpoint:litellm/ai/main:glm-5.2").await;
        let name = key_name("0195f3a2", "litellm/ai/main");
        providers.provision(&on(Some(&main))).await.unwrap();
        keys.secrets
            .lock()
            .unwrap()
            .insert(format!("{name}-key"), "sk-first".into());
        let first = providers
            .target(&on(Some(&main)))
            .await
            .unwrap()
            .unwrap()
            .key_id;
        // Away to another proxy and back: the first key is deleted, and the operator mints another under its name.
        providers
            .release("0195f3a2", std::slice::from_ref(&main))
            .await
            .unwrap();
        keys.secrets
            .lock()
            .unwrap()
            .insert(format!("{name}-key"), "sk-second".into());
        providers.provision(&on(Some(&main))).await.unwrap();
        let second = providers
            .target(&on(Some(&main)))
            .await
            .unwrap()
            .unwrap()
            .key_id;
        assert!(first.starts_with(&format!("litellm:{name}:")));
        assert_ne!(first, second, "each key's spend is its own");
    }
}
