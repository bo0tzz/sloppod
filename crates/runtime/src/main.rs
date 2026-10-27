mod acp;
mod adapter;
mod api;
mod authz;
mod config;
mod credentials;
mod db;
mod enforcer;
mod github;
mod link;
mod mcp;
mod memini;
mod placement;
mod pods;
mod policies;
mod proposals;
mod session;
mod tls;
mod users;
mod vault;

use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use clap::Parser;
use k8s_openapi::api::core::v1::Service;
use kube::api::Api;
use sloppod_contracts::{AgentAdapter, Enforcer, PlacementBackend};
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::credentials::Credentials;
use crate::enforcer::Cilium;
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
    pub credentials: Arc<Credentials>,
    pub policies: Policies,
    pub proposals: ProposalKinds,
    pub placement: Arc<dyn PlacementBackend>,
    pub enforcer: Arc<dyn Enforcer>,
    pub cilium: Arc<Cilium>,
    pub ca: tls::Ca,
    pub authz_addr: SocketAddr,
    pub adapter: Arc<dyn AgentAdapter>,
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
    /// The memory namespace for a session about `repo`; sessions without a project get a holding namespace (ADR 8).
    /// A repository's namespace is shared by everyone working on it; other sessions stay under their owner's home.
    pub fn memory_namespace(&self, repo: Option<&str>, home: &str) -> String {
        let prefix = &self.config.memini_namespace_prefix;
        match repo {
            Some(repo) => format!("{prefix}{repo}"),
            None => format!("{prefix}{home}/unsorted"),
        }
    }

    /// (Re)writes the egress listener; its certificate has to cover every intercepted host.
    pub async fn install_listener(&self) -> anyhow::Result<()> {
        self.cilium
            .install_listener(
                &self.ca,
                &self.policies.hosts(),
                self.authz_addr,
                &self.config.envoy_trusted_ca,
            )
            .await
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

    let vault = vault::Vault::from_base64(&config.secret_key).context("loading the master key")?;
    let credentials = Arc::new(Credentials::new(db.clone(), vault).await?);

    let mut policies = Policies::default();
    policies.register(Arc::new(ModelPolicy::new(credentials.clone())));
    policies.register(Arc::new(GitHubPolicy::new(credentials.clone())));
    policies.register(Arc::new(MeminiPolicy::new(credentials.clone())));

    let mut proposals = ProposalKinds::default();
    proposals.register(Arc::new(proposals::PullRequest::new(credentials.clone())));
    proposals.register(Arc::new(proposals::RepoAccess::new(
        db.clone(),
        credentials.clone(),
    )));

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
        config.sessions_listen.port(),
    ));
    let authz_addr = SocketAddr::new(cluster_ip.parse()?, config.authz_listen.port());

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
        credentials,
        policies,
        proposals,
        placement,
        enforcer: cilium.clone(),
        cilium,
        ca,
        authz_addr,
        adapter: Arc::new(adapter::ClaudeCode),
        instructions,
    }));
    state
        .install_listener()
        .await
        .context("installing the egress listener")?;

    reconcile(&state).await;

    let authz = tokio::spawn(authz::serve(config.authz_listen, state.clone()));
    let ui =
        ServeDir::new(&config.ui_dir).fallback(ServeFile::new(config.ui_dir.join("index.html")));
    let app = Router::new()
        .nest("/api/v1", api::routes())
        .route("/acp", get(acp::handler))
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(ui)
        .with_state(state.clone());
    let sessions = Router::new()
        .route(sloppod_protocol::link::PATH, get(link::handler))
        .route("/mcp", post(mcp::handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let sessions_listener = tokio::net::TcpListener::bind(config.sessions_listen).await?;
    tracing::info!(
        listen = %config.listen,
        sessions = %config.sessions_listen,
        authz = %config.authz_listen,
        "sloppod runtime up"
    );
    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let sessions = axum::serve(
        sessions_listener,
        sessions.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        result = http => result?,
        result = sessions => result?,
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
