//! The session pod's entrypoint (ADR 17): connects outward to the runtime, prepares the pod and bridges the
//! agent's ACP stdio over the link.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use sloppod_protocol::link::{FromSupervisor, Launch, PATH, ToSupervisor, TranscriptFile};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const SYSTEM_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, env = "SLOPPOD_URL")]
    url: String,

    #[arg(
        long,
        env = "SLOPPOD_TOKEN_FILE",
        default_value = "/var/run/secrets/sloppod/token"
    )]
    token_file: PathBuf,

    #[arg(long, env = "SLOPPOD_WORKDIR", default_value = "/workspace")]
    workdir: PathBuf,
}

enum Ending {
    /// The link dropped or the agent exited; reconnect.
    Reconnect,
    /// The pod is being terminated.
    Terminate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("installing the rustls crypto provider"))?;
    let args = Args::parse();
    let mut terminate = signal(SignalKind::terminate())?;
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match run(&args, &mut terminate).await {
            Ok(Ending::Terminate) => return Ok(()),
            Ok(Ending::Reconnect) => tracing::info!("link ended; reconnecting"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = terminate.recv() => return Ok(()),
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn link_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| base.to_owned());
    format!("{base}{PATH}")
}

async fn run(args: &Args, terminate: &mut tokio::signal::unix::Signal) -> anyhow::Result<Ending> {
    let token = std::fs::read_to_string(&args.token_file).context("reading the pod's token")?;
    let mut request = link_url(&args.url).into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {}", token.trim()).parse()?);
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(256 << 20))
        .max_frame_size(Some(256 << 20));
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(request, Some(config), false).await?;
    let (mut sink, mut stream) = socket.split();

    let (out, mut outgoing) = mpsc::unbounded_channel::<FromSupervisor>();
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let send = |message: FromSupervisor| {
        let _ = out.send(message);
    };
    send(FromSupervisor::Hello {
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });

    let launch = loop {
        let Some(message) = stream.next().await else {
            bail!("link closed before launch");
        };
        if let Message::Text(text) = message?
            && let ToSupervisor::Launch(launch) = serde_json::from_str(&text)?
        {
            break *launch;
        }
    };

    let env = match prepare(args, &launch, &send).await {
        Ok(env) => env,
        Err(e) => {
            send(FromSupervisor::Error {
                message: format!("preparing the pod: {e:#}"),
            });
            drop(out);
            let _ = writer.await;
            return Err(e);
        }
    };

    let spawned = Command::new(&launch.command[0])
        .args(&launch.command[1..])
        .envs(env)
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {:?} in {}", launch.command, launch.cwd));
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            send(FromSupervisor::Error {
                message: format!("{e:#}"),
            });
            drop(out);
            let _ = writer.await;
            return Err(e);
        }
    };
    let mut stdin = child.stdin.take().context("agent stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("agent stdout")?).lines();
    send(FromSupervisor::AgentStarted);
    tracing::info!(session = %launch.session_id, "agent started");

    let ending = loop {
        tokio::select! {
            line = stdout.next_line() => match line {
                Ok(Some(line)) => {
                    if line.trim().is_empty() { continue; }
                    match serde_json::from_str(&line) {
                        Ok(message) => send(FromSupervisor::Acp { message }),
                        Err(_) => tracing::warn!(line = %line.chars().take(200).collect::<String>(), "non-JSON line from agent"),
                    }
                }
                Ok(None) | Err(_) => {
                    let status = child.wait().await.ok();
                    send(FromSupervisor::AgentExited { code: status.and_then(|s| s.code()) });
                    sync_transcript(&launch, &send);
                    break Ending::Reconnect;
                }
            },
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ToSupervisor>(&text) {
                    Ok(ToSupervisor::Acp { message }) => {
                        let mut line = serde_json::to_vec(&message)?;
                        line.push(b'\n');
                        stdin.write_all(&line).await?;
                        stdin.flush().await?;
                    }
                    Ok(ToSupervisor::SyncTranscript) => sync_transcript(&launch, &send),
                    Ok(ToSupervisor::Launch(_)) => {}
                    Err(e) => tracing::warn!(error = %e, "bad message from runtime"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break Ending::Reconnect,
                Some(Ok(_)) => {}
            },
            _ = terminate.recv() => {
                tracing::info!("terminating");
                stop(&mut child).await;
                sync_transcript(&launch, &send);
                break Ending::Terminate;
            }
        }
    };
    stop(&mut child).await;
    drop(out);
    let _ = tokio::time::timeout(Duration::from_secs(10), writer).await;
    Ok(ending)
}

async fn stop(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await;
        if tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .is_ok()
        {
            return;
        }
    }
    let _ = child.kill().await;
}

fn sync_transcript(launch: &Launch, send: &impl Fn(FromSupervisor)) {
    let Ok(entries) = std::fs::read_dir(&launch.transcript_dir) else {
        return;
    };
    let files: Vec<TranscriptFile> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let content = std::fs::read(e.path()).ok()?;
            Some(TranscriptFile {
                name: e.file_name().to_string_lossy().into_owned(),
                content_b64: base64::engine::general_purpose::STANDARD.encode(content),
            })
        })
        .collect();
    if !files.is_empty() {
        send(FromSupervisor::Transcript { files });
    }
}

fn write_file(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// Prepares the pod for the agent and returns its environment.
async fn prepare(
    args: &Args,
    launch: &Launch,
    send: &impl Fn(FromSupervisor),
) -> anyhow::Result<Vec<(String, String)>> {
    let home =
        std::env::var("HOME").unwrap_or_else(|_| args.workdir.join("home").display().to_string());
    std::fs::create_dir_all(&home)?;
    let mut env: Vec<(String, String)> = Vec::new();
    let path = std::env::var("PATH").unwrap_or_default();
    env.push((
        "PATH".into(),
        format!("{home}/.local/share/mise/shims:{home}/.local/bin:{path}"),
    ));
    env.push(("MISE_YES".into(), "1".into()));
    env.push(("GIT_CONFIG_COUNT".into(), "2".into()));
    env.push(("GIT_CONFIG_KEY_0".into(), "credential.helper".into()));
    env.push((
        "GIT_CONFIG_VALUE_0".into(),
        "!f() { echo username=x-access-token; echo password=sloppod-placeholder; }; f".into(),
    ));
    env.push(("GIT_CONFIG_KEY_1".into(), "push.default".into()));
    env.push(("GIT_CONFIG_VALUE_1".into(), "current".into()));

    if let Some(ca) = &launch.ca_pem {
        let dir = args.workdir.join(".sloppod");
        let ca_file = dir.join("ca.crt");
        let bundle_file = dir.join("ca-bundle.crt");
        write_file(&ca_file, ca.as_bytes())?;
        let mut bundle = std::fs::read(SYSTEM_BUNDLE).unwrap_or_default();
        bundle.push(b'\n');
        bundle.extend_from_slice(ca.as_bytes());
        write_file(&bundle_file, &bundle)?;
        let bundle = bundle_file.display().to_string();
        env.push(("NODE_EXTRA_CA_CERTS".into(), ca_file.display().to_string()));
        for var in [
            "SSL_CERT_FILE",
            "CURL_CA_BUNDLE",
            "GIT_SSL_CAINFO",
            "REQUESTS_CA_BUNDLE",
            "CARGO_HTTP_CAINFO",
        ] {
            env.push((var.into(), bundle.clone()));
        }
    }
    env.extend(launch.env.iter().cloned());

    for file in &launch.files {
        let path = Path::new(&file.path);
        if file.overwrite || !path.exists() {
            write_file(path, file.content.as_bytes())?;
        }
    }

    std::fs::create_dir_all(&launch.transcript_dir)?;
    // Sessions without a repository work in an empty directory of their own.
    if launch.repo.is_none() {
        std::fs::create_dir_all(&launch.cwd)?;
    }
    for file in &launch.transcript {
        let path = Path::new(&launch.transcript_dir).join(&file.name);
        if !path.exists() {
            let content = base64::engine::general_purpose::STANDARD.decode(&file.content_b64)?;
            write_file(&path, &content)?;
            tracing::info!(file = %file.name, "restored transcript");
        }
    }

    if let Some(repo) = &launch.repo {
        let dir = Path::new(&repo.path);
        if !dir.join(".git").exists() {
            send(FromSupervisor::Status {
                message: format!("cloning {}", repo.clone_url),
            });
            run_cmd(&env, &home, "git", &["clone", &repo.clone_url, &repo.path]).await?;
            let remote_branch = format!("refs/heads/{}", repo.branch);
            let exists = Command::new("git")
                .envs(env.iter().cloned())
                .current_dir(dir)
                .args([
                    "ls-remote",
                    "--exit-code",
                    "--heads",
                    "origin",
                    &remote_branch,
                ])
                .stdout(Stdio::null())
                .status()
                .await?
                .success();
            if exists {
                run_cmd(
                    &env,
                    &repo.path,
                    "git",
                    &["fetch", "origin", &remote_branch],
                )
                .await?;
                run_cmd(
                    &env,
                    &repo.path,
                    "git",
                    &["checkout", "-B", &repo.branch, "FETCH_HEAD"],
                )
                .await?;
            } else {
                run_cmd(&env, &repo.path, "git", &["checkout", "-b", &repo.branch]).await?;
            }
        }
        if launch.install_tools && has_mise_config(dir) {
            send(FromSupervisor::Status {
                message: "installing tools with mise".into(),
            });
            let result = async {
                run_cmd(&env, &repo.path, "mise", &["trust", "--yes", "--all"]).await?;
                run_cmd(&env, &repo.path, "mise", &["install"]).await
            }
            .await;
            if let Err(e) = result {
                send(FromSupervisor::Status {
                    message: format!("mise install failed: {e:#}"),
                });
            }
        }
    }
    Ok(env)
}

fn has_mise_config(dir: &Path) -> bool {
    [
        "mise.toml",
        ".mise.toml",
        "mise/config.toml",
        ".mise/config.toml",
        ".tool-versions",
    ]
    .iter()
    .any(|f| dir.join(f).exists())
}

async fn run_cmd(
    env: &[(String, String)],
    cwd: &str,
    program: &str,
    args: &[&str],
) -> anyhow::Result<()> {
    let output = Command::new(program)
        .args(args)
        .envs(env.iter().cloned())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
