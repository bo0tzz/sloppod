mod acp;
mod adapter;
mod api;
mod authz;
mod config;
mod db;
mod enforcer;
mod github;
mod link;
mod mcp;
mod placement;
mod pods;
mod policies;
mod proposals;
mod session;
mod tls;

use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use clap::Parser;
use k8s_openapi::api::core::v1::Service;
use kube::api::Api;
use sloppod_contracts::{AgentAdapter, Enforcer, PlacementBackend, SessionContext};
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::enforcer::Cilium;
use crate::github::GitHubApp;
use crate::pods::PodIndex;
use crate::policies::{GitHubPolicy, MeminiPolicy, ModelPolicy, Policies};
use crate::proposals::ProposalKinds;
use crate::session::Hub;

pub struct Inner {
    pub config: Config,
    pub db: PgPool,
    pub kube: kube::Client,
    pub pods: PodIndex,
    pub hub: Arc<Hub>,
    pub github: Arc<GitHubApp>,
    pub policies: Policies,
    pub proposals: ProposalKinds,
    pub placement: Arc<dyn PlacementBackend>,
    pub enforcer: Arc<dyn Enforcer>,
    pub adapter: Arc<dyn AgentAdapter>,
    pub ca_pem: String,
    pub memini_url: Option<String>,
    pub instructions: Option<String>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Inner {
    pub fn memory_namespace(&self, session: &db::SessionRow) -> String {
        format!("{}{}", self.config.memini_namespace_prefix, session.repo)
    }

    pub fn session_context(&self, session: &db::SessionRow) -> SessionContext {
        SessionContext {
            session_id: session.id.to_string(),
            repo: session.repo.clone(),
            branch: session.branch.clone(),
            memory_namespace: self
                .memini_url
                .as_ref()
                .map(|_| self.memory_namespace(session)),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sloppod=debug".into()),
        )
        .init();
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("installing the rustls crypto provider"))?;
    let config = Config::parse();

    let db = db::connect(&config.database_url)
        .await
        .context("connecting to Postgres")?;
    let kube = kube::Client::try_default()
        .await
        .context("connecting to Kubernetes")?;
    let pods = PodIndex::start(kube.clone(), &config.namespace);

    let key = std::fs::read(&config.github_app_private_key_file)
        .context("reading the GitHub App private key")?;
    let github = Arc::new(GitHubApp::new(config.github_app_id.clone(), &key)?);

    let mut policies = Policies::default();
    policies.register(Arc::new(ModelPolicy::new(
        config.claude_oauth_token.clone(),
    )));
    policies.register(Arc::new(GitHubPolicy::new(github.clone())));
    let memini_url = match (&config.memini_url, &config.memini_api_key) {
        (Some(url), Some(key)) => {
            let host = reqwest::Url::parse(url)?
                .host_str()
                .context("memini URL without host")?
                .to_owned();
            policies.register(Arc::new(MeminiPolicy::new(host, key.clone())));
            Some(url.clone())
        }
        _ => None,
    };

    let mut proposals = ProposalKinds::default();
    proposals.register(Arc::new(proposals::PullRequest::new(github.clone())));

    let ca = tls::Ca::load_or_create(kube.clone(), &config.namespace, &config.ca_secret).await?;
    let service: Service = Api::namespaced(kube.clone(), &config.namespace)
        .get(&config.service)
        .await
        .with_context(|| format!("reading the runtime's Service {}", config.service))?;
    let cluster_ip = service
        .spec
        .and_then(|s| s.cluster_ip)
        .context("runtime Service without a cluster IP")?;
    let cilium = Arc::new(Cilium::new(
        &kube,
        &config.namespace,
        config.runtime_labels(),
        config.listen.port(),
    ));
    cilium
        .install_listener(
            &ca,
            &policies.hosts(),
            SocketAddr::new(cluster_ip.parse()?, config.authz_listen.port()),
            &config.envoy_trusted_ca,
        )
        .await
        .context("installing the egress listener")?;

    let placement = Arc::new(placement::AgentSandbox::new(
        kube.clone(),
        &config.namespace,
        config.session_service_account.clone(),
        config.session_image_pull_policy.clone(),
    ));
    let instructions = match &config.instructions_file {
        Some(path) => Some(std::fs::read_to_string(path).context("reading instructions")?),
        None => None,
    };

    let state = AppState(Arc::new(Inner {
        hub: Arc::new(Hub::new(db.clone())),
        config: config.clone(),
        db,
        kube,
        pods,
        github,
        policies,
        proposals,
        placement,
        enforcer: cilium,
        adapter: Arc::new(adapter::ClaudeCode),
        ca_pem: ca.cert_pem.clone(),
        memini_url,
        instructions,
    }));

    reconcile(&state).await;

    let authz = tokio::spawn(authz::serve(config.authz_listen, state.clone()));
    let ui =
        ServeDir::new(&config.ui_dir).fallback(ServeFile::new(config.ui_dir.join("index.html")));
    let app = Router::new()
        .nest("/api/v1", api::routes())
        .route("/acp", get(acp::handler))
        .route(sloppod_protocol::link::PATH, get(link::handler))
        .route("/mcp", post(mcp::handler))
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(ui)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(listen = %config.listen, authz = %config.authz_listen, "sloppod runtime up");
    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        result = http => result?,
        result = authz => result??,
    }
    Ok(())
}

/// Brings session pods and policies back in line with the database after a restart.
async fn reconcile(state: &AppState) {
    let sessions = match db::active_sessions(&state.db).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(error = %e, "listing sessions to reconcile");
            return;
        }
    };
    for session in sessions {
        state
            .hub
            .set_phase(session.id, sloppod_protocol::api::Phase::Disconnected)
            .await;
        if let Err(e) = api::place(state, &session).await {
            tracing::error!(session = %session.id, error = %e, "reconciling session");
        }
    }
}
