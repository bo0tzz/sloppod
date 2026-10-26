use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

const DEFAULT_DIRECT_HOSTS: &str = "static.rust-lang.org,sh.rustup.rs,index.crates.io,static.crates.io,crates.io,\
objects.githubusercontent.com,release-assets.githubusercontent.com,raw.githubusercontent.com,codeload.github.com,\
registry.npmjs.org,nodejs.org,pypi.org,files.pythonhosted.org,mise.jdx.dev,mise-versions.jdx.dev,\
dl.k8s.io,get.helm.sh,proxy.golang.org,sum.golang.org,storage.googleapis.com";

#[derive(Debug, Clone, Parser)]
#[command(version, about)]
pub struct Config {
    #[arg(long, env = "SLOPPOD_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

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

    /// Labels selecting runtime pods, for session egress policy (`k=v,k=v`).
    #[arg(
        long,
        env = "SLOPPOD_RUNTIME_SELECTOR",
        default_value = "app.kubernetes.io/name=sloppod"
    )]
    pub runtime_selector: String,

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

    /// Long-lived Claude OAuth token (`claude setup-token`). Never enters session pods.
    #[arg(long, env = "SLOPPOD_CLAUDE_OAUTH_TOKEN", hide_env_values = true)]
    pub claude_oauth_token: String,

    #[arg(long, env = "SLOPPOD_GITHUB_APP_ID")]
    pub github_app_id: String,

    #[arg(long, env = "SLOPPOD_GITHUB_APP_PRIVATE_KEY_FILE")]
    pub github_app_private_key_file: PathBuf,

    #[arg(long, env = "SLOPPOD_MEMINI_URL")]
    pub memini_url: Option<String>,

    #[arg(long, env = "SLOPPOD_MEMINI_API_KEY", hide_env_values = true)]
    pub memini_api_key: Option<String>,

    /// Prefix for every memory namespace, e.g. `dev/` to keep a test install's memories apart.
    #[arg(long, env = "SLOPPOD_MEMINI_NAMESPACE_PREFIX", default_value = "")]
    pub memini_namespace_prefix: String,

    /// Instructions rendered into every session as the agent's user-level instructions.
    #[arg(long, env = "SLOPPOD_INSTRUCTIONS_FILE")]
    pub instructions_file: Option<PathBuf>,

    #[arg(long, env = "SLOPPOD_AGENT_MODEL")]
    pub agent_model: Option<String>,

    /// ACP mode new sessions start in, e.g. `default`, `acceptEdits`, `bypassPermissions`.
    #[arg(long, env = "SLOPPOD_DEFAULT_MODE")]
    pub default_mode: Option<String>,

    /// Hosts sessions may reach directly, without the authorizer (package registries).
    #[arg(long, env = "SLOPPOD_DIRECT_HOSTS", value_delimiter = ',', default_value = DEFAULT_DIRECT_HOSTS)]
    pub direct_hosts: Vec<String>,

    /// Git identity for commits made in sessions.
    #[arg(long, env = "SLOPPOD_GIT_NAME", default_value = "sloppod")]
    pub git_name: String,

    #[arg(
        long,
        env = "SLOPPOD_GIT_EMAIL",
        default_value = "sloppod@users.noreply.github.com"
    )]
    pub git_email: String,

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
    pub fn runtime_url(&self) -> String {
        format!(
            "http://{}.{}.svc:{}",
            self.service,
            self.namespace,
            self.listen.port()
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
