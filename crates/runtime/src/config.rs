use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

const DEFAULT_DIRECT_HOSTS: &str = "static.rust-lang.org,sh.rustup.rs,index.crates.io,static.crates.io,crates.io,\
objects.githubusercontent.com,release-assets.githubusercontent.com,raw.githubusercontent.com,codeload.github.com,\
registry.npmjs.org,nodejs.org,pypi.org,files.pythonhosted.org,mise.jdx.dev,mise-versions.jdx.dev,\
dl.k8s.io,get.helm.sh,proxy.golang.org,sum.golang.org,storage.googleapis.com,\
results-receiver.actions.githubusercontent.com,pipelines.actions.githubusercontent.com,productionresultssa*.blob.core.windows.net,\
cdn.playwright.dev,playwright.download.prss.microsoft.com";

#[derive(Debug, Clone, Parser)]
#[command(version, about)]
pub struct Config {
    #[arg(long, env = "SLOPPOD_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// The supervisor link and the MCP endpoint, the only things session pods may reach. Kept off the API's port
    /// so an agent can't use the API, e.g. to approve its own proposals.
    #[arg(long, env = "SLOPPOD_SESSIONS_LISTEN", default_value = "0.0.0.0:8081")]
    pub sessions_listen: SocketAddr,

    /// Envoy `ext_authz` gRPC listener.
    #[arg(long, env = "SLOPPOD_AUTHZ_LISTEN", default_value = "0.0.0.0:9001")]
    pub authz_listen: SocketAddr,

    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// Namespace the runtime and its session pods live in.
    #[arg(long, env = "SLOPPOD_NAMESPACE", default_value = "sloppod")]
    pub namespace: String,

    /// The runtime's Service, used by session pods and by Envoy.
    #[arg(long, env = "SLOPPOD_SERVICE", default_value = "sloppod")]
    pub service: String,

    /// The cluster's DNS domain. Sessions may look up names under it, and otherwise only hosts they may reach.
    #[arg(long, env = "SLOPPOD_CLUSTER_DOMAIN", default_value = "cluster.local")]
    pub cluster_domain: String,

    /// Labels selecting runtime pods, for session egress policy (`k=v,k=v`).
    #[arg(
        long,
        env = "SLOPPOD_RUNTIME_SELECTOR",
        default_value = "app.kubernetes.io/name=sloppod"
    )]
    pub runtime_selector: String,

    /// Secret holding the egress CA; created on first start.
    #[arg(long, env = "SLOPPOD_CA_SECRET", default_value = "sloppod-egress-ca")]
    pub ca_secret: String,

    #[arg(
        long,
        env = "SLOPPOD_SESSION_IMAGE",
        default_value = "ghcr.io/bo0tzz/sloppod-session:latest"
    )]
    pub session_image: String,

    #[arg(
        long,
        env = "SLOPPOD_SESSION_IMAGE_PULL_POLICY",
        default_value = "IfNotPresent"
    )]
    pub session_image_pull_policy: String,

    #[arg(
        long,
        env = "SLOPPOD_SESSION_SERVICE_ACCOUNT",
        default_value = "sloppod-session"
    )]
    pub session_service_account: String,

    #[arg(long, env = "SLOPPOD_SESSION_STORAGE_SIZE", default_value = "20Gi")]
    pub session_storage_size: String,

    #[arg(long, env = "SLOPPOD_SESSION_STORAGE_CLASS")]
    pub session_storage_class: Option<String>,

    #[arg(long, env = "SLOPPOD_SESSION_CPU")]
    pub session_cpu: Option<String>,

    #[arg(long, env = "SLOPPOD_SESSION_MEMORY", default_value = "4Gi")]
    pub session_memory: Option<String>,

    /// 32 bytes, base64: encrypts stored credentials (ADR 21). Losing it makes them unreadable.
    #[arg(long, env = "SLOPPOD_SECRET_KEY", hide_env_values = true)]
    pub secret_key: String,

    /// Header the authenticating proxy puts the user's name in (ADR 23).
    #[arg(long, env = "SLOPPOD_USER_HEADER", default_value = "x-forwarded-user")]
    pub user_header: String,

    /// User for requests without the header; unset, such requests are refused.
    #[arg(long, env = "SLOPPOD_DEFAULT_USER")]
    pub default_user: Option<String>,

    /// Prefix for every memory namespace, e.g. `dev/` to keep a test install's memories apart.
    #[arg(long, env = "SLOPPOD_MEMINI_NAMESPACE_PREFIX", default_value = "")]
    pub memini_namespace_prefix: String,

    /// What this runtime was built from (a release tag or commit); agents can read it in their pod.
    #[arg(long, env = "SLOPPOD_BUILD")]
    pub build: Option<String>,

    /// Instructions rendered into every session as the agent's user-level instructions.
    #[arg(long, env = "SLOPPOD_INSTRUCTIONS_FILE")]
    pub instructions_file: Option<PathBuf>,

    /// Hosts sessions may reach directly, without the authorizer (package registries).
    #[arg(long, env = "SLOPPOD_DIRECT_HOSTS", value_delimiter = ',', default_value = DEFAULT_DIRECT_HOSTS)]
    pub direct_hosts: Vec<String>,

    #[arg(long, env = "SLOPPOD_UI_DIR", default_value = "/usr/share/sloppod/ui")]
    pub ui_dir: PathBuf,

    /// CA bundle inside Cilium's Envoy for verifying real upstreams.
    #[arg(
        long,
        env = "SLOPPOD_ENVOY_TRUSTED_CA",
        default_value = "/etc/ssl/certs/ca-certificates.crt"
    )]
    pub envoy_trusted_ca: String,
}

impl Config {
    /// How session pods reach the runtime.
    pub fn runtime_url(&self) -> String {
        format!(
            "http://{}.{}.svc:{}",
            self.service,
            self.namespace,
            self.sessions_listen.port()
        )
    }

    pub fn runtime_labels(&self) -> Vec<(String, String)> {
        self.runtime_selector
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect()
    }
}
