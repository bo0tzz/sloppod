//! The per-request authorizer: Envoy's `ext_authz` gRPC API (ADR 11).

use std::collections::BTreeMap;
use std::net::SocketAddr;

use envoy_types::ext_authz::v3::pb::{
    Authorization, AuthorizationServer, CheckRequest, CheckResponse, HeaderAppendAction,
    HttpStatusCode,
};
use envoy_types::ext_authz::v3::{
    CheckRequestExt, CheckResponseExt, DeniedHttpResponseBuilder, OkHttpResponseBuilder,
};
use sloppod_contracts::{Decision, HttpRequest};
use tonic::{Request, Response, Status};

use crate::{AppState, db};

pub struct Authorizer {
    state: AppState,
}

pub async fn serve(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    tonic::transport::Server::builder()
        .add_service(AuthorizationServer::new(Authorizer { state }))
        .serve(addr)
        .await?;
    Ok(())
}

fn deny(message: &str) -> CheckResponse {
    deny_with(403, message, None)
}

fn deny_with(status: u16, message: &str, content_type: Option<&str>) -> CheckResponse {
    let code = match status {
        200 => HttpStatusCode::Ok,
        401 => HttpStatusCode::Unauthorized,
        404 => HttpStatusCode::NotFound,
        429 => HttpStatusCode::TooManyRequests,
        503 => HttpStatusCode::ServiceUnavailable,
        _ => HttpStatusCode::Forbidden,
    };
    let mut denied = DeniedHttpResponseBuilder::new();
    denied
        .set_http_status(code)
        .add_header(
            "content-type",
            content_type.unwrap_or("text/plain; charset=utf-8"),
            None,
            false,
        )
        .set_body(message);
    let mut response = CheckResponse::with_status(Status::permission_denied(message.trim()));
    response.set_http_response(denied);
    response
}

impl Authorizer {
    async fn decide(&self, check: CheckRequest) -> CheckResponse {
        let Some(ip) = check.get_client_address().cloned() else {
            return deny("sloppod: request without a source address.\n");
        };
        let Some(http) = check
            .attributes
            .as_ref()
            .and_then(|a| a.request.as_ref())
            .and_then(|r| r.http.as_ref())
        else {
            return deny("sloppod: not an HTTP request.\n");
        };
        let host = http
            .host
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let Some(session_id) = self.state.pods.session_for_ip(&ip) else {
            tracing::warn!(%ip, %host, "request from an unknown pod");
            return deny("sloppod: request from a pod that isn't a known session.\n");
        };
        let session = match db::get_session(&self.state.db, session_id).await {
            Ok(Some(session)) => session,
            _ => return deny("sloppod: unknown session.\n"),
        };
        let Some(policy) = self.state.policies.for_host(&host) else {
            return deny(&format!("sloppod: no request policy for {host}.\n"));
        };
        let headers: BTreeMap<String, String> = http
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        let body_truncated = headers
            .get("x-envoy-auth-partial-body")
            .is_some_and(|v| v == "true");
        let body = if http.raw_body.is_empty() {
            http.body.clone().into_bytes()
        } else {
            http.raw_body.clone()
        };
        let request = HttpRequest {
            method: http.method.clone(),
            host: host.clone(),
            path: http.path.clone(),
            headers,
            body,
            body_truncated,
        };
        let context = self.state.session_context(&session);
        let decision = match policy.decide(&context, &request).await {
            Ok(decision) => decision,
            Err(e) => {
                tracing::error!(session = %session_id, %host, error = %e, "request policy failed");
                return deny_with(
                    503,
                    &format!("sloppod: the {host} policy failed: {e}\n"),
                    None,
                );
            }
        };
        match decision {
            Decision::Allow {
                set_headers,
                remove_headers,
            } => {
                tracing::info!(session = %session_id, method = %request.method, %host, path = %request.path_only(), "allow");
                let mut ok = OkHttpResponseBuilder::new();
                for name in remove_headers {
                    if !set_headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case(&name))
                    {
                        ok.remove_header(name);
                    }
                }
                for (name, value) in set_headers {
                    ok.add_header(
                        name,
                        value,
                        Some(HeaderAppendAction::OverwriteIfExistsOrAdd),
                        false,
                    );
                }
                let mut response = CheckResponse::with_status(Status::ok("allowed"));
                response.set_http_response(ok);
                response
            }
            Decision::Deny {
                status,
                message,
                content_type,
            } => {
                tracing::info!(session = %session_id, method = %request.method, %host, path = %request.path_only(), status, "deny");
                deny_with(status, &message, content_type.as_deref())
            }
        }
    }
}

#[tonic::async_trait]
impl Authorization for Authorizer {
    async fn check(
        &self,
        request: Request<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        Ok(Response::new(self.decide(request.into_inner()).await))
    }
}
