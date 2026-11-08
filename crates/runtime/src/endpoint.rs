//! Sessions on a model of an Anthropic-compatible endpoint (#461): a LiteLLM proxy, or anything else that speaks the
//! Messages API. Claude Code is pointed at the runtime's `/inference` route on the sessions port, which sends its
//! requests on to the endpoint with the endpoint's key attached, so the endpoint can live at any address the runtime
//! reaches, a cluster Service included, and the pod never holds the key.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Value, json};
use sloppod_contracts::{
    Decision, Grant, HttpRequest, ModelProvider, ModelTarget, OfferedModel, RequestPolicy,
    SessionContext, capability,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::credentials::{Account, CredentialRow, Credentials, ENDPOINT};
use crate::{AppState, db};

/// Where sessions reach their endpoint on the runtime's sessions port.
pub const PREFIX: &str = "/inference";
/// How an endpoint's model is named wherever a model is chosen: `endpoint:<location>:<model>`, the location naming the
/// endpoint in its provider's terms (an endpoint credential's id, a LiteLLM proxy).
const ID_PREFIX: &str = "endpoint:";
/// A request body the runtime takes: a long conversation with images is several megabytes.
const MAX_BODY: usize = 64 << 20;
/// How long a model's answer may go quiet before it's given up on. There's no limit on how long it takes in all: a
/// long answer streams for minutes, and a thinking model can be silent for a while before it starts.
pub const IDLE: Duration = Duration::from_secs(600);
/// How long a runtime that's stopping waits for model answers in flight to finish (see [`Streams::drain`]).
pub const DRAIN: Duration = Duration::from_secs(60);

/// One of an endpoint's models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointModel {
    pub location: String,
    pub model: String,
}

impl EndpointModel {
    /// The model a value names, if it's an endpoint's. A location never has a `:`; a model's name may.
    pub fn parse(value: &str) -> Option<Self> {
        let (location, model) = value.strip_prefix(ID_PREFIX)?.split_once(':')?;
        if location.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            location: location.to_owned(),
            model: model.to_owned(),
        })
    }

    /// How it's named wherever a model is chosen.
    pub fn id(&self) -> String {
        format!("{ID_PREFIX}{}:{}", self.location, self.model)
    }
}

/// The model providers registered (ADR 7): the owner's own endpoints, and the LiteLLM operator's proxies where the
/// deployment watches them. Everything the core does with endpoint models goes through here, by location.
#[derive(Default)]
pub struct Providers(Vec<Arc<dyn ModelProvider>>);

impl Providers {
    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) {
        self.0.push(provider);
    }

    fn of_location(&self, location: &str) -> Option<&Arc<dyn ModelProvider>> {
        self.0.iter().find(|p| p.owns(location))
    }

    /// The location a session's endpoint grant names. A grant made before grants carried one names the endpoint
    /// credential it's for.
    fn location(grant: &Grant) -> Option<String> {
        grant.param("location").map(str::to_owned).or_else(|| {
            crate::grants::accounts(grant)?
                .first()
                .map(|id| (*id).to_owned())
        })
    }

    fn of_grant(&self, grant: &Grant) -> Option<&Arc<dyn ModelProvider>> {
        self.of_location(&Self::location(grant)?)
    }

    /// The models every provider offers the owner. A provider that fails to say is left out, and logged.
    pub async fn offered(&self, owner: &str) -> Vec<OfferedModel> {
        let mut offered = Vec::new();
        for provider in &self.0 {
            match provider.models(owner).await {
                Ok(models) => offered.extend(models),
                Err(e) => {
                    tracing::warn!(provider = %provider.manifest().name, error = %e.0, "listing an endpoint provider's models")
                }
            }
        }
        offered
    }

    /// The context window of the endpoint model `value` (its id) in tokens, when its provider knows it: a LiteLLM
    /// model's `maxInputTokens`, or the one given with an endpoint credential.
    pub async fn context_window(&self, owner: &str, value: &str) -> Option<u64> {
        self.offered(owner)
            .await
            .into_iter()
            .find(|m| m.value == value)
            .and_then(|m| m.info["maxInputTokens"].as_u64())
    }

    /// Which endpoint model `value` is: one named by its id, which has to be offered to the owner, or by its bare
    /// name when exactly one endpoint offered to them serves a model of that name. `None` for any other value, which
    /// is a model of the agent's own; an error, for the user or agent, for one that can't be used.
    pub async fn resolve(&self, owner: &str, value: &str) -> Result<Option<EndpointModel>, String> {
        let offered = self.offered(owner).await;
        if let Some(wanted) = EndpointModel::parse(value) {
            return if offered.iter().any(|m| m.value == value) {
                Ok(Some(wanted))
            } else {
                Err(format!(
                    "{} isn't a model of an endpoint you have (any more)",
                    wanted.model
                ))
            };
        }
        if value.starts_with(ID_PREFIX) {
            return Err(format!("{value} isn't an endpoint model's id"));
        }
        let matching: Vec<EndpointModel> = offered
            .iter()
            .filter(|m| m.model == value)
            .filter_map(|m| EndpointModel::parse(&m.value))
            .collect();
        match matching.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            _ => Err(format!(
                "more than one of your endpoints serves {value}; name it by its id (endpoint:<endpoint>:{value})"
            )),
        }
    }

    /// The `endpoint` grant a session of the owner's on `model` gets (#461).
    pub async fn grant(
        &self,
        owner: &str,
        model: &EndpointModel,
    ) -> anyhow::Result<Result<Grant, String>> {
        let Some(provider) = self.of_location(&model.location) else {
            return Ok(Err(format!(
                "{} isn't an endpoint you have",
                model.location
            )));
        };
        Ok(provider
            .grant(owner, &model.location, &model.model)
            .await
            .map_err(|e| anyhow::anyhow!(e.0))?
            .map(|mut params| {
                params["location"] = json!(model.location);
                Grant::new(capability::ENDPOINT, params)
            }))
    }

    /// Makes ready what the session's endpoint grant needs, e.g. a key minted for it.
    pub async fn provision(&self, session: &SessionContext) -> anyhow::Result<()> {
        for grant in session
            .grants
            .iter()
            .filter(|g| g.capability == capability::ENDPOINT)
        {
            if let Some(provider) = self.of_grant(grant) {
                provider
                    .provision(session, grant)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.0))?;
            }
        }
        Ok(())
    }

    /// Removes what provisioning made for these grants of a session's. Every grant is tried; the first failure is
    /// returned, so a purge tries again, and the sweep is the backstop for the rest.
    pub async fn release(&self, session_id: &str, grants: &[Grant]) -> anyhow::Result<()> {
        let mut failed = None;
        for grant in grants
            .iter()
            .filter(|g| g.capability == capability::ENDPOINT)
        {
            if let Some(provider) = self.of_grant(grant)
                && let Err(e) = provider.release(session_id, grant).await
            {
                tracing::warn!(session = session_id, error = %e.0, "releasing a session's endpoint key");
                failed.get_or_insert(anyhow::anyhow!(e.0));
            }
        }
        failed.map_or(Ok(()), Err)
    }

    /// Has every provider remove what it made for sessions that don't hold it any more (`keep`: each endpoint grant
    /// still held, by session id).
    pub async fn sweep(&self, keep: &[(String, Grant)]) {
        for provider in &self.0 {
            if let Err(e) = provider.sweep(keep).await {
                tracing::warn!(provider = %provider.manifest().name, error = %e.0, "sweeping endpoint keys no session holds");
            }
        }
    }

    /// Where the session's model requests go, and with which key.
    pub async fn target(&self, session: &SessionContext) -> anyhow::Result<Option<ModelTarget>> {
        let Some(grant) = session
            .grants
            .iter()
            .find(|g| g.capability == capability::ENDPOINT)
        else {
            return Ok(None);
        };
        let Some(provider) = self.of_grant(grant) else {
            return Ok(None);
        };
        provider
            .target(session, grant)
            .await
            .map_err(|e| anyhow::anyhow!(e.0))
    }
}

/// The `endpoint` grant a session of the owner's on `model` gets, with the owner's budget per session where they set
/// one, for providers whose keys keep one (#461).
pub async fn session_grant(
    state: &AppState,
    owner: &str,
    model: &EndpointModel,
) -> anyhow::Result<Result<Grant, String>> {
    let budget = db::user_session_budget(&state.db, owner).await?;
    Ok(state.providers.grant(owner, model).await?.map(|mut grant| {
        if let Some(budget) = budget {
            grant.params["max_budget"] = json!(budget);
        }
        grant
    }))
}
/// A session's grants with its endpoint grant following its model: the one its model needs, or none (#461). Every
/// session gets it this way, a sub-session too (ADR 27, 2026-11-07): a model gives no reach into anything else, so a
/// child's isn't limited to its parent's.
pub fn follow_model(grants: &mut Vec<Grant>, endpoint: Option<Grant>) {
    grants.retain(|g| g.capability != capability::ENDPOINT);
    grants.extend(endpoint);
}

/// The models an endpoint credential serves.
pub fn models_of(endpoint: &CredentialRow) -> Vec<String> {
    endpoint.config["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The owner's own endpoints, as credentials (ADR 21): each located by its credential's id, its key the secret.
pub struct OwnEndpoints {
    credentials: Arc<Credentials>,
}

impl OwnEndpoints {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

fn contract_error(e: impl std::fmt::Display) -> sloppod_contracts::Error {
    sloppod_contracts::Error::new(e.to_string())
}

#[async_trait::async_trait]
impl ModelProvider for OwnEndpoints {
    fn manifest(&self) -> sloppod_contracts::Manifest {
        sloppod_contracts::Manifest::builtin("endpoint-credential")
    }

    fn owns(&self, location: &str) -> bool {
        location.parse::<Uuid>().is_ok()
    }

    async fn models(&self, owner: &str) -> sloppod_contracts::Result<Vec<OfferedModel>> {
        let endpoints = self.credentials.list(owner).await.map_err(contract_error)?;
        Ok(endpoints
            .iter()
            .filter(|row| row.kind == ENDPOINT)
            .flat_map(|row| {
                models_of(row).into_iter().map(|model| OfferedModel {
                    value: EndpointModel {
                        location: row.id.to_string(),
                        model: model.clone(),
                    }
                    .id(),
                    name: format!("{model} ({})", row.name),
                    // In LiteLLM's terms, as the operator's models carry it.
                    info: match row.config["context_windows"][&model].as_u64() {
                        Some(tokens) => json!({"maxInputTokens": tokens}),
                        None => Value::Null,
                    },
                    model,
                })
            })
            .collect())
    }

    async fn grant(
        &self,
        owner: &str,
        location: &str,
        model: &str,
    ) -> sloppod_contracts::Result<Result<Value, String>> {
        let row = self
            .credentials
            .owned(owner, ENDPOINT, location)
            .await
            .map_err(contract_error)?;
        Ok(match row {
            Some(row) if models_of(&row).iter().any(|m| m == model) => {
                Ok(json!({"credentials": [row.id.to_string()]}))
            }
            Some(row) => Err(format!(
                "the endpoint {} doesn't list the model {model}",
                row.name
            )),
            None => Err("that endpoint isn't one of yours any more".into()),
        })
    }

    async fn target(
        &self,
        session: &SessionContext,
        _grant: &Grant,
    ) -> sloppod_contracts::Result<Option<ModelTarget>> {
        let row = match self
            .credentials
            .account(session, ENDPOINT)
            .await
            .map_err(contract_error)?
        {
            Account::Found(row) => row,
            Account::Gone | Account::None => return Ok(None),
        };
        let base_url = row
            .config("url")
            .ok_or_else(|| contract_error("the endpoint credential has no URL"))?
            .trim_end_matches('/')
            .to_owned();
        let key = self.credentials.reveal(&row).map_err(contract_error)?;
        Ok(Some(ModelTarget {
            base_url,
            key,
            // Every session on the endpoint uses its one key.
            key_id: format!("credential:{}", row.id),
            shared: Some(row.name.clone()),
        }))
    }
}

/// The model inference streams in flight through the runtime, so a runtime that's stopping can let them finish.
#[derive(Default)]
pub struct Streams {
    open: AtomicUsize,
    draining: AtomicBool,
    closed: Notify,
}

/// One stream in flight, counted until it's dropped: when the answer has been sent, or the session went away.
pub struct InFlight(Arc<Streams>);

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.0.open.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.closed.notify_waiters();
        }
    }
}

impl Streams {
    /// Counts a new stream, unless the runtime is stopping: then the request is refused, and the agent tries it
    /// again, against the runtime that comes up next.
    pub fn enter(self: &Arc<Self>) -> Option<InFlight> {
        if self.draining.load(Ordering::SeqCst) {
            return None;
        }
        self.open.fetch_add(1, Ordering::SeqCst);
        Some(InFlight(self.clone()))
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// Stops taking new streams and waits for those in flight to finish, for up to `bound`. Says how many were
    /// still open when it gave up waiting.
    pub async fn drain(&self, bound: Duration) -> usize {
        self.draining.store(true, Ordering::SeqCst);
        let finished = async {
            loop {
                let closed = self.closed.notified();
                if self.open() == 0 {
                    return;
                }
                closed.await;
            }
        };
        let _ = tokio::time::timeout(bound, finished).await;
        self.open()
    }
}

/// A response whose body holds `in_flight` until it's been sent or dropped.
pub fn counted(response: Response, in_flight: InFlight) -> Response {
    response.map(|body| {
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            let _held = &in_flight;
            chunk
        }))
    })
}

pub async fn proxy(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let Some(in_flight) = state.inference_streams.enter() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "sloppod: the runtime is restarting; try again in a moment\n",
        )
            .into_response();
    };
    match forward(&state, peer, request).await {
        Ok(response) => counted(response, in_flight),
        Err(e) => {
            tracing::warn!(%peer, error = %format!("{e:#}"), "proxying to a model endpoint");
            (
                StatusCode::BAD_GATEWAY,
                format!("sloppod: the model endpoint isn't reachable: {e:#}\n"),
            )
                .into_response()
        }
    }
}

async fn forward(state: &AppState, peer: SocketAddr, request: Request) -> anyhow::Result<Response> {
    let Some(session) = crate::identity::calling_session(state, request.headers(), peer.ip()).await
    else {
        return Ok((
            StatusCode::FORBIDDEN,
            "sloppod: model endpoints are only for session pods\n",
        )
            .into_response());
    };
    let session = db::get_session(&state.db, session)
        .await?
        .context("unknown session")?;
    if let Some(why) = session.not_acting() {
        return Ok((
            StatusCode::FORBIDDEN,
            format!("sloppod: this session is {why}, so it has no model\n"),
        )
            .into_response());
    }
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, MAX_BODY)
        .await
        .context("reading the request body")?;
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    send(
        Target {
            policy: state.endpoint.as_ref(),
            providers: &state.providers,
            reach: &state.inference_reach,
            user_header: &state.config.user_header,
        },
        &session.context(),
        parts.method,
        path,
        &parts.headers,
        body,
    )
    .await
}

/// What a session's request to its endpoint goes through.
pub struct Target<'a> {
    pub policy: &'a dyn RequestPolicy,
    pub providers: &'a Providers,
    pub reach: &'a crate::proxy::Reach,
    pub user_header: &'a str,
}

/// Sends a session's request (its path still under [`PREFIX`]) to its endpoint, if the endpoint policy allows it,
/// with the key the policy attaches; the answer streams back as it comes.
pub async fn send(
    target: Target<'_>,
    session: &SessionContext,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> anyhow::Result<Response> {
    let path = match path_and_query.strip_prefix(PREFIX) {
        Some(rest) if rest.starts_with('/') => rest.to_owned(),
        Some(rest) => format!("/{rest}"),
        None => path_and_query.to_owned(),
    };
    let Ok(path) = crate::proxy::canonical_path(&path) else {
        return Ok((StatusCode::BAD_REQUEST, "sloppod: invalid path\n").into_response());
    };
    // The policy refuses a session whose endpoint is gone.
    let base = target
        .providers
        .target(session)
        .await?
        .map(|t| t.base_url.trim_end_matches('/').to_owned())
        .unwrap_or_default();
    let header_map: BTreeMap<String, String> = headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let decision = target
        .policy
        .decide(
            session,
            &HttpRequest {
                method: method.to_string(),
                host: reqwest::Url::parse(&base)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default(),
                path: path.clone(),
                headers: header_map,
                body: body.to_vec(),
                body_truncated: false,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.0))?;
    let (set, remove) = match decision {
        Decision::Allow {
            set_headers,
            remove_headers,
        } => sloppod_contracts::header_ops(set_headers, remove_headers),
        Decision::Deny {
            status,
            message,
            content_type,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
            let content_type = content_type.unwrap_or_else(|| "text/plain; charset=utf-8".into());
            return Ok((status, [(header::CONTENT_TYPE, content_type)], message).into_response());
        }
    };
    let url = format!("{base}{path}");
    target
        .reach
        .forward(crate::proxy::Forward {
            method,
            headers,
            url: &url,
            body,
            set,
            remove,
            user_header: target.user_header,
        })
        .await
}

/// The model a new session starts on, checked (#461): an endpoint's model as its id, whether it was named by its id
/// or its bare name, and only for an agent that runs on endpoints; any other model as it was given.
pub async fn chosen(
    providers: &Providers,
    owner: &str,
    agent: &dyn sloppod_contracts::AgentAdapter,
    model: Option<&str>,
) -> sqlx::Result<Result<Option<String>, String>> {
    let Some(model) = model else {
        return Ok(Ok(None));
    };
    Ok(match providers.resolve(owner, model).await {
        Ok(Some(endpoint)) if agent.takes_endpoints() => Ok(Some(endpoint.id())),
        Ok(Some(endpoint)) => Err(format!(
            "{} can't run on an endpoint's model like {}",
            agent.agent(),
            endpoint.model
        )),
        Ok(None) => Ok(Some(model.to_owned())),
        Err(why) => Err(why),
    })
}

/// Whether a session can move onto its agent's own provider (`model` being one of its models): it holds the agent's
/// inference grant, and an account that grant names is still there, as the authorizer would pick it (ADR 29). A
/// default the grant doesn't name doesn't count.
pub async fn own_provider_ready(
    credentials: &Credentials,
    session: &SessionContext,
    agent: &dyn sloppod_contracts::AgentAdapter,
    model: &str,
) -> sqlx::Result<Result<(), String>> {
    let needed = agent.credential();
    let inference = agent.inference_capability();
    if !session.grants.iter().any(|g| g.capability == inference) {
        return Ok(Err(format!(
            "{model} needs the {inference} grant, which this session doesn't hold; choose another of your \
             endpoints' models"
        )));
    }
    Ok(match credentials.account(session, &needed.kind).await? {
        Account::Found(_) => Ok(()),
        Account::Gone => Err(format!(
            "the accounts this session's {inference} grant names are gone; give it another in the grants list \
             first, or choose another of your endpoints' models"
        )),
        Account::None => Err(format!(
            "{model} needs {}; add one in Settings, or choose another of your endpoints' models",
            needed.name
        )),
    })
}

/// Whether two endpoint grants are on the same endpoint, so what one provisioned is the other's too.
fn same_endpoint(a: Option<&Grant>, b: Option<&Grant>) -> bool {
    match (
        a.and_then(Providers::location),
        b.and_then(Providers::location),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Puts back what provisioning a switch from `old` to `new` changed, once its record is back: the old grant's key as
/// it was (a key on the same endpoint now limited to the new model, say), and the new grant's own, if it made one.
pub async fn undo_provisioning(
    providers: &Providers,
    old_context: &SessionContext,
    old: Option<&Grant>,
    new: Option<&Grant>,
) -> anyhow::Result<()> {
    if old.is_some() {
        providers.provision(old_context).await?;
    }
    if let Some(new) = new
        && !same_endpoint(old, Some(new))
    {
        providers
            .release(&old_context.session_id, std::slice::from_ref(new))
            .await?;
    }
    Ok(())
}

/// A change of a session's model that involves an endpoint's (#461), to one or from one: unlike a change between the
/// agent's own models, the agent can't make it itself, since it's another provider (ADR 20). The model and the
/// endpoint grant change together, and the agent starts again on a new pod, resuming its conversation. So that a pick
/// doesn't restart the agent under the user, it's only recorded here, checked as far as it can be, and the session
/// moves onto it before its next message is sent ([`apply_pending`]). Picking the current model again takes a
/// recorded pick back. `None` when neither the old model nor the new is an endpoint's: the agent switches as usual,
/// and a pick that was recorded is dropped.
pub async fn choose(
    state: &AppState,
    session: Uuid,
    value: &str,
) -> Option<Result<Value, crate::link::RpcError>> {
    state
        .hub
        .one_switch_at_a_time(session, async {
            let row = match session_row(state, session).await {
                Ok(row) => row,
                Err(e) => return Some(Err(e)),
            };
            let picked = match prepare(state, &row, value).await {
                Ok(picked) => picked,
                Err(e) => return Some(Err(e)),
            };
            let pending = match &picked {
                Some(move_to) if Some(&move_to.model) != row.model.as_ref() => {
                    Some(move_to.model.as_str())
                }
                _ => None,
            };
            if pending != row.pending_model.as_deref() {
                if let Err(e) = db::set_pending_model(&state.db, session, pending).await {
                    return Some(Err(crate::link::RpcError::new(-32603, e.to_string())));
                }
                state.hub.broadcast_session(session).await;
            }
            picked.map(|_| Ok(json!({})))
        })
        .await
}

/// Moves the session onto the model the user picked, if there's one waiting ([`choose`]), before a message is sent
/// to it. A session that isn't quiet, with a turn or its queue still running, keeps the pick for its next message. A
/// move that fails drops the pick, leaves the session on its model and fails the message, so it isn't answered by a
/// model the user didn't expect.
pub async fn apply_pending(state: &AppState, session: Uuid) -> Result<(), crate::link::RpcError> {
    use crate::link::RpcError;
    state
        .hub
        .one_switch_at_a_time(session, async {
            let row = session_row(state, session).await?;
            let Some(pending) = row.pending_model.clone() else {
                return Ok(());
            };
            let moved = match prepare(state, &row, &pending).await {
                Ok(Some(move_to)) => move_to
                    .apply(state, &row)
                    .await
                    .map_err(|e| format!("{e:#}")),
                Ok(None) => Ok(true),
                Err(e) => Err(e.message),
            };
            match moved {
                Ok(false) => Ok(()),
                Ok(true) => {
                    // A move clears the pick with the model; one there was nothing to do for is cleared here.
                    db::set_pending_model(&state.db, session, None)
                        .await
                        .map_err(|e| RpcError::new(-32603, e.to_string()))
                }
                Err(why) => {
                    if let Err(e) = db::set_pending_model(&state.db, session, None).await {
                        tracing::warn!(%session, error = %e, "dropping a model pick that failed");
                    }
                    state.hub.broadcast_session(session).await;
                    Err(RpcError::new(
                        -32603,
                        format!(
                            "the message wasn't sent: the switch to {pending} failed, and the session stays on {}: {why}",
                            row.model.as_deref().unwrap_or("its model")
                        ),
                    ))
                }
            }
        })
        .await
}

async fn session_row(
    state: &AppState,
    session: Uuid,
) -> Result<db::SessionRow, crate::link::RpcError> {
    use crate::link::RpcError;
    match db::get_session(&state.db, session).await {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(RpcError::new(-32602, "no such session")),
        Err(e) => Err(RpcError::new(-32603, e.to_string())),
    }
}

/// A move of a session onto another provider's model, checked: the model as the session will record it, and the
/// endpoint grant it needs.
struct MoveTo {
    model: String,
    grant: Option<Grant>,
}

/// What `value` would move the session onto, if it involves an endpoint's model, checked against what the session
/// would need there; `None` when neither its model nor `value` is an endpoint's.
async fn prepare(
    state: &AppState,
    row: &db::SessionRow,
    value: &str,
) -> Result<Option<MoveTo>, crate::link::RpcError> {
    use crate::link::RpcError;
    let Some(adapter) = state.agents.get(&row.agent) else {
        return Ok(None);
    };
    let resolved = chosen(&state.providers, &row.owner, adapter.as_ref(), Some(value))
        .await
        .map_err(|e| RpcError::new(-32603, e.to_string()))?;
    let was = EndpointModel::parse(row.model.as_deref().unwrap_or_default());
    let model = match resolved {
        Ok(Some(model)) => model,
        Ok(None) => value.to_owned(),
        Err(why) => return Err(RpcError::new(-32602, why)),
    };
    let to = EndpointModel::parse(&model);
    if was.is_none() && to.is_none() {
        return Ok(None);
    }
    // Off an endpoint, onto the agent's own provider: the session needs a usable account there, by its own grant.
    if to.is_none() {
        match own_provider_ready(&state.credentials, &row.context(), adapter.as_ref(), &model).await
        {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return Err(RpcError::new(-32602, why)),
            Err(e) => return Err(RpcError::new(-32603, e.to_string())),
        }
    }
    let grant = match &to {
        Some(to) => match session_grant(state, &row.owner, to).await {
            Ok(Ok(grant)) => Some(grant),
            Ok(Err(why)) => return Err(RpcError::new(-32602, why)),
            Err(e) => return Err(RpcError::new(-32603, format!("{e:#}"))),
        },
        None => None,
    };
    Ok(Some(MoveTo { model, grant }))
}

impl MoveTo {
    /// Moves the session: its model and endpoint grant change together, and its agent starts again on a new pod. Says
    /// whether it moved; `false` when the session isn't quiet, and nothing changed. If any part fails, all of it is
    /// put back.
    async fn apply(self, state: &AppState, row: &db::SessionRow) -> anyhow::Result<bool> {
        let MoveTo {
            model,
            grant: new_grant,
        } = self;
        let session = row.id;
        let old_grant = row
            .grants()
            .into_iter()
            .find(|g| g.capability == capability::ENDPOINT);
        let old_model = row.model.clone().unwrap_or_default();
        let id = session.to_string();
        // The session as it'll be, for provisioning its new grant (a key minted for it, say) before its agent starts,
        // and as it was, for putting it back.
        let old_context = row.context();
        let mut moved_context = row.context();
        follow_model(&mut moved_context.grants, new_grant.clone());
        let moved = state
            .hub
            .switch_provider(
                session,
                || async {
                    db::set_model_and_endpoint(&state.db, session, &model, new_grant.as_ref())
                        .await?;
                    state.providers.provision(&moved_context).await?;
                    Ok(())
                },
                || async {
                    state.placement.replace_pod(&id).await?;
                    Ok(())
                },
                || async {
                    db::set_model_and_endpoint(&state.db, session, &old_model, old_grant.as_ref())
                        .await?;
                    undo_provisioning(
                        &state.providers,
                        &old_context,
                        old_grant.as_ref(),
                        new_grant.as_ref(),
                    )
                    .await
                },
            )
            .await?;
        if !moved {
            return Ok(false);
        }
        // What the old grant had made is no longer needed, unless the new one is on the same endpoint: what its key
        // spent since the last read is read first, and a release that fails is left to the sweep.
        if let Some(old) = &old_grant
            && !same_endpoint(Some(old), new_grant.as_ref())
        {
            crate::spend::read_last(state, &old_context).await;
            if let Err(e) = state
                .providers
                .release(&id, std::slice::from_ref(old))
                .await
            {
                tracing::warn!(%session, error = %format!("{e:#}"), "releasing a session's old endpoint key; the sweep tries again");
            }
        }
        state.hub.broadcast_session(session).await;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use base64::Engine;
    use sloppod_protocol::api::NewCredential;

    use super::*;
    use crate::policies::EndpointPolicy;

    #[test]
    fn an_endpoint_model_is_named_by_its_endpoint_and_name() {
        let model = EndpointModel {
            location: "litellm/ai/main".into(),
            model: "qwen/qwen3:32b".into(),
        };
        assert_eq!(model.id(), "endpoint:litellm/ai/main:qwen/qwen3:32b");
        assert_eq!(EndpointModel::parse(&model.id()), Some(model.clone()));
        for other in [
            "sonnet",
            "endpoint:",
            "endpoint:nope",
            "endpoint::qwen3",
            "endpoint:litellm/ai/main:",
        ] {
            assert_eq!(EndpointModel::parse(other), None, "{other}");
        }
        // The grant follows the model, replacing one for another endpoint, or going.
        let mut grants = vec![
            Grant::new(capability::MODEL, json!({})),
            Grant::new(capability::ENDPOINT, json!({"location": "old"})),
        ];
        let new = Grant::new(capability::ENDPOINT, json!({"location": "litellm/ai/main"}));
        follow_model(&mut grants, Some(new.clone()));
        assert_eq!(grants, [Grant::new(capability::MODEL, json!({})), new]);
        follow_model(&mut grants, None);
        assert_eq!(grants, [Grant::new(capability::MODEL, json!({}))]);
    }

    #[tokio::test]
    async fn a_stopping_runtime_waits_for_answers_in_flight_for_a_while() {
        let streams = Arc::new(Streams::default());
        let quick = streams.enter().unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(quick);
        });
        assert_eq!(streams.drain(Duration::from_secs(5)).await, 0);
        // New ones are refused once it's stopping.
        assert!(streams.enter().is_none());

        let streams = Arc::new(Streams::default());
        let _held = streams.enter().unwrap();
        let started = Instant::now();
        assert_eq!(
            streams.drain(Duration::from_millis(100)).await,
            1,
            "gave up waiting"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// What the mock endpoint saw: each request's path and its `authorization` and `x-api-key` headers.
    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    /// An Anthropic-compatible endpoint on loopback that streams its answer slowly: an event every `gap`, `events`
    /// of them.
    async fn endpoint(gap: Duration, events: usize) -> (String, Seen) {
        let seen: Seen = Default::default();
        let saw = seen.clone();
        let app = axum::Router::new().fallback(move |request: Request| {
            let saw = saw.clone();
            async move {
                let header = |name: &str| {
                    request
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned()
                };
                saw.lock().unwrap().push((
                    request.uri().to_string(),
                    header("authorization"),
                    header("x-api-key"),
                ));
                let stream = futures::stream::unfold(0, move |n| async move {
                    if n == events {
                        return None;
                    }
                    if n > 0 {
                        tokio::time::sleep(gap).await;
                    }
                    let event = format!("event: content_block_delta\ndata: {{\"n\":{n}}}\n\n");
                    Some((Ok::<_, std::convert::Infallible>(Bytes::from(event)), n + 1))
                });
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream),
                )
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    async fn credentials_with(db: sqlx::PgPool, url: &str) -> (Arc<Credentials>, Uuid) {
        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let credentials = Arc::new(
            Credentials::new(db, crate::vault::Vault::from_base64(&key).unwrap())
                .await
                .unwrap(),
        );
        let mut config = serde_json::Map::new();
        config.insert("url".into(), json!(url));
        config.insert("models".into(), json!("qwen3-coder, glm-5"));
        let new = NewCredential {
            kind: ENDPOINT.into(),
            name: "litellm".into(),
            config,
            secret: "sk-endpoint".into(),
        };
        let id = credentials.create("dev", new).await.unwrap().unwrap().id;
        (credentials, id)
    }

    fn session(grants: Vec<Grant>) -> SessionContext {
        SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants,
            role: Default::default(),
        }
    }

    fn providers_of(credentials: &Arc<Credentials>) -> Arc<Providers> {
        let mut providers = Providers::default();
        providers.register(Arc::new(OwnEndpoints::new(credentials.clone())));
        Arc::new(providers)
    }

    async fn ask(
        credentials: &Arc<Credentials>,
        reach: &crate::proxy::Reach,
        session: &SessionContext,
        method: Method,
        path: &str,
    ) -> Response {
        let providers = providers_of(credentials);
        let policy = EndpointPolicy::new(providers.clone());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", crate::adapter::PLACEHOLDER)
                .parse()
                .unwrap(),
        );
        send(
            Target {
                policy: &policy,
                providers: &providers,
                reach,
                user_header: "x-sloppod-user",
            },
            session,
            method,
            path,
            &headers,
            Bytes::from_static(b"{\"model\":\"qwen3-coder\",\"stream\":true}"),
        )
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn a_session_s_requests_reach_its_endpoint_with_the_key_and_stream_back(
        db: sqlx::PgPool,
    ) {
        // An answer that takes 2 s in all, an event every 250 ms: longer than the idle limit, which it never reaches.
        let (url, seen) = endpoint(Duration::from_millis(250), 9).await;
        let (credentials, id) = credentials_with(db, &url).await;
        let reach = crate::proxy::Reach::local(Duration::from_millis(1000));
        let on_endpoint = session(vec![Grant::new(
            capability::ENDPOINT,
            json!({"credentials": [id.to_string()]}),
        )]);

        let started = Instant::now();
        let response = ask(
            &credentials,
            &reach,
            &on_endpoint,
            Method::POST,
            "/inference/v1/messages?beta=true",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        // Passed on as it comes, not once it's all in.
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        assert!(String::from_utf8_lossy(&first).contains("\"n\":0"));
        let mut events = 1;
        while let Some(chunk) = body.next().await {
            events += String::from_utf8_lossy(&chunk.unwrap())
                .matches("event:")
                .count();
        }
        assert_eq!(events, 9);
        assert!(started.elapsed() >= Duration::from_secs(2));
        // The endpoint got the key, and never the placeholder.
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            [(
                "/v1/messages?beta=true".to_owned(),
                "Bearer sk-endpoint".to_owned(),
                "sk-endpoint".to_owned()
            )]
        );

        // Only inference, and only for a session on the endpoint.
        let response = ask(
            &credentials,
            &reach,
            &on_endpoint,
            Method::GET,
            "/inference/v1/models",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = ask(
            &credentials,
            &reach,
            &session(vec![]),
            Method::POST,
            "/inference/v1/messages",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(seen.lock().unwrap().len(), 1);

        // Its endpoint deleted, the session is refused, never given another.
        credentials.delete("dev", id).await.unwrap();
        let response = ask(
            &credentials,
            &reach,
            &on_endpoint,
            Method::POST,
            "/inference/v1/messages",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn an_answer_that_goes_quiet_for_too_long_is_given_up_on(db: sqlx::PgPool) {
        let (url, _) = endpoint(Duration::from_millis(800), 3).await;
        let (credentials, id) = credentials_with(db, &url).await;
        let reach = crate::proxy::Reach::local(Duration::from_millis(300));
        let on_endpoint = session(vec![Grant::new(
            capability::ENDPOINT,
            json!({"credentials": [id.to_string()]}),
        )]);
        let response = ask(
            &credentials,
            &reach,
            &on_endpoint,
            Method::POST,
            "/inference/v1/messages",
        )
        .await;
        let mut body = response.into_body().into_data_stream();
        assert!(body.next().await.unwrap().is_ok());
        assert!(
            body.next().await.unwrap().is_err(),
            "the stream ends with an error"
        );
    }

    #[sqlx::test]
    async fn an_endpoint_model_is_one_of_the_owner_s_by_its_id_or_name(db: sqlx::PgPool) {
        let (credentials, id) =
            credentials_with(db, "http://litellm.ai.svc.cluster.local:4000").await;
        let providers = providers_of(&credentials);
        let model = |name: &str| EndpointModel {
            location: id.to_string(),
            model: name.into(),
        };
        let qwen = model("qwen3-coder");
        assert_eq!(
            providers.resolve("dev", &qwen.id()).await,
            Ok(Some(qwen.clone()))
        );
        assert_eq!(
            providers.resolve("dev", "qwen3-coder").await,
            Ok(Some(qwen.clone()))
        );
        assert_eq!(providers.resolve("dev", "sonnet").await, Ok(None));
        assert!(providers.resolve("someone", &qwen.id()).await.is_err());
        assert!(
            providers
                .resolve("dev", &model("llama").id())
                .await
                .is_err()
        );
        let offered: Vec<(String, String, String)> = providers
            .offered("dev")
            .await
            .into_iter()
            .map(|m| (m.value, m.name, m.model))
            .collect();
        assert_eq!(
            offered,
            [
                (
                    qwen.id(),
                    "qwen3-coder (litellm)".to_owned(),
                    "qwen3-coder".to_owned()
                ),
                (
                    model("glm-5").id(),
                    "glm-5 (litellm)".to_owned(),
                    "glm-5".to_owned()
                ),
            ]
        );
        // Its grant names the endpoint, both as its location and as the credential it acts with (ADR 29).
        assert_eq!(
            providers.grant("dev", &qwen).await.unwrap().unwrap().params,
            json!({"location": id.to_string(), "credentials": [id.to_string()]})
        );
        assert!(providers.grant("someone", &qwen).await.unwrap().is_err());
        // Only for an agent that runs on endpoints.
        let claude = crate::adapter::ClaudeCode;
        assert_eq!(
            chosen(&providers, "dev", &claude, Some("qwen3-coder"))
                .await
                .unwrap(),
            Ok(Some(qwen.id()))
        );
        assert!(
            chosen(
                &providers,
                "dev",
                &crate::adapter::Codex,
                Some("qwen3-coder")
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(
            chosen(&providers, "dev", &claude, Some("opus"))
                .await
                .unwrap(),
            Ok(Some("opus".into()))
        );
    }

    #[sqlx::test]
    async fn moving_back_to_claude_needs_an_account_the_session_s_grant_names(db: sqlx::PgPool) {
        let (credentials, _) = credentials_with(db, "http://litellm:4000").await;
        let claude = |name: &str| NewCredential {
            kind: crate::credentials::CLAUDE.into(),
            name: name.into(),
            config: Default::default(),
            secret: format!("sk-{name}"),
        };
        let first = credentials
            .create("dev", claude("first"))
            .await
            .unwrap()
            .unwrap();
        let on = |grants: Vec<Grant>| session(grants);
        let named = on(vec![Grant::new(
            capability::MODEL,
            json!({"credentials": [first.id.to_string()]}),
        )]);
        let agent = crate::adapter::ClaudeCode;
        assert_eq!(
            own_provider_ready(&credentials, &named, &agent, "opus")
                .await
                .unwrap(),
            Ok(())
        );
        // Its account deleted and another made the default: the grant still names only the one that's gone.
        credentials.delete("dev", first.id).await.unwrap();
        credentials
            .create("dev", claude("second"))
            .await
            .unwrap()
            .unwrap();
        let refused = own_provider_ready(&credentials, &named, &agent, "opus")
            .await
            .unwrap()
            .unwrap_err();
        assert!(refused.contains("are gone"), "{refused}");
        // Without the model grant at all, neither.
        assert!(
            own_provider_ready(&credentials, &on(vec![]), &agent, "opus")
                .await
                .unwrap()
                .is_err()
        );
    }
}
