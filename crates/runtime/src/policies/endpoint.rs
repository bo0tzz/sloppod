use std::sync::Arc;

use async_trait::async_trait;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::capabilities::{Part, Provider, holds};
use crate::endpoint::Providers;

/// An Anthropic-compatible endpoint (#461): inference only, with the key of the endpoint the session's grant names,
/// as its model provider gives it. Sessions reach it through the runtime's `/inference` route, not the egress
/// listener.
pub struct EndpointPolicy {
    providers: Arc<Providers>,
}

impl EndpointPolicy {
    pub fn new(providers: Arc<Providers>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl RequestPolicy for EndpointPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("model-endpoint")
    }

    /// None: the endpoint can be anywhere the runtime reaches, a cluster Service included, so sessions reach it
    /// through the runtime.
    fn hosts(&self) -> Vec<String> {
        Vec::new()
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        if !holds(session, &Part::Model(Provider::Endpoint)) {
            return Ok(Decision::deny(
                "sloppod: this session isn't on an endpoint's model.\n",
            ));
        }
        if !super::model::is_inference(request) {
            return Ok(Decision::deny(format!(
                "sloppod: sessions may only call the endpoint's inference API (POST /v1/messages); {} {} was \
                 refused.\n",
                request.method,
                request.path_only()
            )));
        }
        let Some(target) = self
            .providers
            .target(session)
            .await
            .map_err(|e| Error::new(format!("{e:#}")))?
        else {
            return Ok(Decision::deny(
                "sloppod: the endpoint this session's model is on, or its key, is gone; the user can choose \
                 another model for it.\n",
            ));
        };
        // Anthropic's API takes `x-api-key`; gateways like LiteLLM take either. The agent's placeholder goes.
        Ok(Decision::allow_with(
            vec![
                ("authorization".into(), format!("Bearer {}", target.key)),
                ("x-api-key".into(), target.key),
            ],
            Vec::new(),
        ))
    }
}
