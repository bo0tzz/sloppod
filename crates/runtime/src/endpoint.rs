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
use sloppod_contracts::{Decision, Grant, HttpRequest, RequestPolicy, SessionContext, capability};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::credentials::{Account, CredentialRow, Credentials, ENDPOINT};
use crate::{AppState, db};

/// Where sessions reach their endpoint on the runtime's sessions port.
pub const PREFIX: &str = "/inference";
/// How an endpoint's model is named wherever a model is chosen: `endpoint:<credential>:<model>`.
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
    pub credential: Uuid,
    pub model: String,
}

impl EndpointModel {
    /// The model a value names, if it's an endpoint's.
    pub fn parse(value: &str) -> Option<Self> {
        let (credential, model) = value.strip_prefix(ID_PREFIX)?.split_once(':')?;
        if model.is_empty() {
            return None;
        }
        Some(Self {
            credential: credential.parse().ok()?,
            model: model.to_owned(),
        })
    }

    /// How it's named wherever a model is chosen.
    pub fn id(&self) -> String {
        format!("{ID_PREFIX}{}:{}", self.credential, self.model)
    }
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

/// The owner's endpoints' models, as a model list names them: `{value, name}`, the name saying which endpoint.
pub async fn offered(credentials: &Credentials, owner: &str) -> sqlx::Result<Vec<Value>> {
    let endpoints = credentials.list(owner).await?;
    Ok(endpoints
        .iter()
        .filter(|row| row.kind == ENDPOINT)
        .flat_map(|row| {
            models_of(row).into_iter().map(|model| {
                let id = EndpointModel {
                    credential: row.id,
                    model: model.clone(),
                }
                .id();
                json!({"value": id, "name": format!("{model} ({})", row.name)})
            })
        })
        .collect())
}

/// Which of the owner's endpoint models `value` is: one named by its id, which has to be one of theirs and still
/// listed, or by its bare name when exactly one of their endpoints serves a model of that name. `None` for any other
/// value, which is a model of the agent's own; an error, for the user or agent, for one that can't be used.
pub async fn resolve(
    credentials: &Credentials,
    owner: &str,
    value: &str,
) -> sqlx::Result<Result<Option<EndpointModel>, String>> {
    if let Some(wanted) = EndpointModel::parse(value) {
        let row = credentials
            .owned(owner, ENDPOINT, &wanted.credential.to_string())
            .await?;
        return Ok(match row {
            Some(row) if models_of(&row).contains(&wanted.model) => Ok(Some(wanted)),
            Some(row) => Err(format!(
                "the endpoint {} doesn't list the model {}",
                row.name, wanted.model
            )),
            None => Err("that endpoint isn't one of yours any more".into()),
        });
    }
    if value.starts_with(ID_PREFIX) {
        return Ok(Err(format!("{value} isn't an endpoint model's id")));
    }
    let endpoints = credentials.list(owner).await?;
    let matching: Vec<EndpointModel> = endpoints
        .iter()
        .filter(|row| row.kind == ENDPOINT && models_of(row).iter().any(|m| m == value))
        .map(|row| EndpointModel {
            credential: row.id,
            model: value.to_owned(),
        })
        .collect();
    Ok(match matching.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => Err(format!(
            "more than one of your endpoints serves {value}; name it by its id (endpoint:<endpoint>:{value})"
        )),
    })
}

/// A session's grants with its endpoint grant following its model: the one endpoint its model is on, or none
/// (#461). Every session gets it this way, a sub-session too (ADR 27, 2026-11-07): a model gives no reach into
/// anything else, so a child's isn't limited to its parent's.
pub fn follow_model(grants: &mut Vec<Grant>, model: Option<&EndpointModel>) {
    grants.retain(|g| g.capability != capability::ENDPOINT);
    if let Some(model) = model {
        grants.push(Grant::new(
            capability::ENDPOINT,
            json!({"credentials": [model.credential.to_string()]}),
        ));
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
            credentials: &state.credentials,
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
    pub credentials: &'a Credentials,
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
    let base = match target.credentials.account(session, ENDPOINT).await? {
        Account::Found(row) => row
            .config("url")
            .context("the endpoint credential has no URL")?
            .trim_end_matches('/')
            .to_owned(),
        // The policy says so.
        Account::Gone | Account::None => String::new(),
    };
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
    credentials: &Credentials,
    owner: &str,
    agent: &dyn sloppod_contracts::AgentAdapter,
    model: Option<&str>,
) -> sqlx::Result<Result<Option<String>, String>> {
    let Some(model) = model else {
        return Ok(Ok(None));
    };
    Ok(match resolve(credentials, owner, model).await? {
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

/// A change of a session's model that involves an endpoint's (#461), to one or from one: unlike a change between the
/// agent's own models, the agent can't make it itself, since it's another provider (ADR 20). The model and the
/// endpoint grant change together, and the agent starts again on a new pod, resuming its conversation. `None` when
/// neither the old model nor the new is an endpoint's: the agent switches as usual.
pub async fn switch(
    state: &AppState,
    session: Uuid,
    value: &str,
) -> Option<Result<Value, crate::link::RpcError>> {
    use crate::link::RpcError;
    let row = match db::get_session(&state.db, session).await {
        Ok(Some(row)) => row,
        Ok(None) => return Some(Err(RpcError::new(-32602, "no such session"))),
        Err(e) => return Some(Err(RpcError::new(-32603, e.to_string()))),
    };
    let adapter = state.agents.get(&row.agent)?;
    let resolved = match chosen(
        &state.credentials,
        &row.owner,
        adapter.as_ref(),
        Some(value),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(e) => return Some(Err(RpcError::new(-32603, e.to_string()))),
    };
    let was = EndpointModel::parse(row.model.as_deref().unwrap_or_default());
    let model = match resolved {
        Ok(Some(model)) => model,
        Ok(None) => value.to_owned(),
        Err(why) => return Some(Err(RpcError::new(-32602, why))),
    };
    let to = EndpointModel::parse(&model);
    if was.is_none() && to.is_none() {
        return None;
    }
    // Off an endpoint, onto the agent's own provider: the session needs a usable account there, by its own grant.
    if to.is_none() {
        match own_provider_ready(&state.credentials, &row.context(), adapter.as_ref(), &model).await
        {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return Some(Err(RpcError::new(-32602, why))),
            Err(e) => return Some(Err(RpcError::new(-32603, e.to_string()))),
        }
    }
    let mut grants = Vec::new();
    follow_model(&mut grants, to.as_ref());
    let old_grant = row
        .grants()
        .into_iter()
        .find(|g| g.capability == capability::ENDPOINT);
    let old_model = row.model.clone().unwrap_or_default();
    let id = session.to_string();
    let moved = state
        .hub
        .switch_provider(
            session,
            || async {
                db::set_model_and_endpoint(&state.db, session, &model, grants.first()).await?;
                Ok(())
            },
            || async {
                state.placement.replace_pod(&id).await?;
                Ok(())
            },
            || async {
                db::set_model_and_endpoint(&state.db, session, &old_model, old_grant.as_ref())
                    .await?;
                Ok(())
            },
        )
        .await;
    match moved {
        Ok(true) => {
            state.hub.broadcast_session(session).await;
            Some(Ok(json!({})))
        }
        Ok(false) => Some(Err(RpcError::new(
            -32602,
            "switching to or from an endpoint's model starts the agent again, so it waits until the session is quiet: no turn, nothing queued, no open question and none of its commands running",
        ))),
        Err(e) => Some(Err(RpcError::new(
            -32603,
            format!("the session stays on its model: {e:#}"),
        ))),
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
            credential: Uuid::nil(),
            model: "qwen/qwen3:32b".into(),
        };
        assert_eq!(EndpointModel::parse(&model.id()), Some(model.clone()));
        for other in [
            "sonnet",
            "endpoint:",
            "endpoint:nope:qwen3",
            &format!("endpoint:{}:", Uuid::nil()),
        ] {
            assert_eq!(EndpointModel::parse(other), None, "{other}");
        }
        // The grant follows the model, replacing one for another endpoint, or going.
        let mut grants = vec![
            Grant::new(capability::MODEL, json!({})),
            Grant::new(capability::ENDPOINT, json!({"credentials": ["old"]})),
        ];
        follow_model(&mut grants, Some(&model));
        assert_eq!(grants.len(), 2);
        assert_eq!(
            grants[1].params,
            json!({"credentials": [Uuid::nil().to_string()]})
        );
        follow_model(&mut grants, None);
        assert_eq!(
            grants
                .iter()
                .map(|g| g.capability.as_str())
                .collect::<Vec<_>>(),
            ["model"]
        );
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

    async fn ask(
        credentials: &Arc<Credentials>,
        reach: &crate::proxy::Reach,
        session: &SessionContext,
        method: Method,
        path: &str,
    ) -> Response {
        let policy = EndpointPolicy::new(credentials.clone());
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
                credentials,
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
        let qwen = EndpointModel {
            credential: id,
            model: "qwen3-coder".into(),
        };
        assert_eq!(
            resolve(&credentials, "dev", &qwen.id()).await.unwrap(),
            Ok(Some(qwen.clone()))
        );
        assert_eq!(
            resolve(&credentials, "dev", "qwen3-coder").await.unwrap(),
            Ok(Some(qwen.clone()))
        );
        assert_eq!(
            resolve(&credentials, "dev", "sonnet").await.unwrap(),
            Ok(None)
        );
        assert!(
            resolve(&credentials, "someone", &qwen.id())
                .await
                .unwrap()
                .is_err()
        );
        let unlisted = EndpointModel {
            credential: id,
            model: "llama".into(),
        };
        assert!(
            resolve(&credentials, "dev", &unlisted.id())
                .await
                .unwrap()
                .is_err()
        );
        let offered = offered(&credentials, "dev").await.unwrap();
        assert_eq!(
            offered,
            [
                json!({"value": qwen.id(), "name": "qwen3-coder (litellm)"}),
                json!({"value": EndpointModel { credential: id, model: "glm-5".into() }.id(), "name": "glm-5 (litellm)"}),
            ]
        );
        // Only for an agent that runs on endpoints.
        let claude = crate::adapter::ClaudeCode;
        assert_eq!(
            chosen(&credentials, "dev", &claude, Some("qwen3-coder"))
                .await
                .unwrap(),
            Ok(Some(qwen.id()))
        );
        assert!(
            chosen(
                &credentials,
                "dev",
                &crate::adapter::Codex,
                Some("qwen3-coder")
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(
            chosen(&credentials, "dev", &claude, Some("opus"))
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
