//! The session pod's entrypoint (ADR 17): connects outward to the runtime, prepares the pod and bridges the
//! agent's ACP stdio over the link.

mod proxy;
mod shutdown;

/// Whether the runtime's endpoints are served locally to the agent.
static LOCAL_PROXY_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use clap::Parser;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use sloppod_protocol::link::{
    AgentReady, FromSupervisor, Launch, PATH, RUNTIME_INFO, RunningAgent, ToSupervisor,
    TranscriptRef,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

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

    /// Exit successfully only if the runtime was heard from recently; for the liveness probe.
    #[arg(long)]
    check: bool,
}

enum Ending {
    /// The link dropped or the agent exited; reconnect.
    Reconnect,
    /// The pod is being terminated.
    Terminate,
}

type Sink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// How long the link may be silent before it counts as dead; the runtime pings every 20 s.
const LINK_TIMEOUT: Duration = Duration::from_secs(60);

/// Touched whenever the runtime is heard from; the pod's liveness probe (`--check`) reads it.
const HEARTBEAT: &str = "/tmp/sloppod-link";
/// How long the link may stay down before the pod counts as broken and the kubelet restarts it.
const LINK_GRACE: Duration = Duration::from_secs(600);

fn heartbeat() {
    if let Err(e) = std::fs::write(HEARTBEAT, b"") {
        tracing::warn!(error = %e, "writing the heartbeat");
    }
}

/// The liveness probe: healthy while the runtime was heard from within the grace period.
fn check() -> anyhow::Result<()> {
    let age = std::fs::metadata(HEARTBEAT)?.modified()?.elapsed()?;
    if age > LINK_GRACE {
        bail!("no word from the runtime for {}s", age.as_secs());
    }
    Ok(())
}

/// The agent runs until it exits or the pod is terminated, across links (ADR 17).
struct Agent {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    transcript_dir: String,
    /// Set once the runtime has the agent's session up; until then a new runtime launches afresh.
    ready: Option<AgentReady>,
    /// Requests the runtime sent the agent that it hasn't answered, by id.
    open: HashMap<String, Value>,
    /// Requests the agent sent the runtime that weren't answered, by id.
    waiting: HashMap<String, Value>,
    /// A message from the agent that the last link failed to send.
    unsent: Option<Value>,
    /// How much of each transcript file the runtime has, so syncs send only what was appended.
    synced: HashMap<String, u64>,
    /// The agent's environment and working directory, for commands the runtime runs beside it.
    env: Vec<(String, String)>,
    cwd: String,
}

impl Agent {
    fn running(&self) -> Option<RunningAgent> {
        Some(RunningAgent {
            ready: self.ready.clone()?,
            open: self.open.values().cloned().collect(),
            waiting: self.waiting.values().cloned().collect(),
        })
    }
}

/// Notes a message sent one way: a request the other side now owes an answer to, or an answer it was owed.
fn track(sent: &mut HashMap<String, Value>, owed: &mut HashMap<String, Value>, message: &Value) {
    let Some(id) = message.get("id") else { return };
    if message.get("method").is_some() {
        sent.insert(id.to_string(), message.clone());
    } else {
        owed.remove(&id.to_string());
    }
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
    if args.check {
        return check();
    }
    heartbeat();
    // Without it the agent calls the runtime directly, as before (#196).
    match proxy::bind().await {
        Ok(listener) => {
            let (runtime, token_file) = (args.url.clone(), args.token_file.clone());
            LOCAL_PROXY_UP.store(true, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                if let Err(e) = proxy::serve(listener, runtime, token_file).await {
                    tracing::error!(error = %format!("{e:#}"), "serving the runtime's endpoints locally");
                }
            });
        }
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "binding the local endpoint for the agent");
        }
    }
    let mut terminate = signal(SignalKind::terminate())?;
    let mut agent = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match run(&args, &mut agent, &mut terminate).await {
            Ok(Ending::Terminate) => return Ok(()),
            Ok(Ending::Reconnect) => tracing::info!("link ended; reconnecting"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = terminate.recv() => {
                if let Some(mut agent) = agent.take() {
                    stop(&mut agent.child).await;
                }
                return Ok(());
            }
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

async fn send(sink: &mut Sink, message: &FromSupervisor) -> anyhow::Result<()> {
    let text = serde_json::to_string(message)?;
    sink.send(Message::Text(text.into())).await?;
    Ok(())
}

/// Reads the link until it closes, for while the supervisor is busy with something else.
async fn closed(stream: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) {
    while let Some(Ok(message)) = stream.next().await {
        heartbeat();
        if let Message::Close(_) = message {
            return;
        }
    }
}

async fn run(
    args: &Args,
    agent: &mut Option<Agent>,
    terminate: &mut tokio::signal::unix::Signal,
) -> anyhow::Result<Ending> {
    let token = std::fs::read_to_string(&args.token_file).context("reading the pod's token")?;
    let mut request = link_url(&args.url).into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {}", token.trim()).parse()?);
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(256 << 20))
        .max_frame_size(Some(256 << 20));
    let (socket, _) = tokio::time::timeout(
        LINK_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .context("connecting timed out")??;
    let (mut sink, mut stream) = socket.split();
    heartbeat();

    // An agent that exited while the link was down isn't read from then, so check before saying it's running.
    if let Some(a) = agent.as_mut()
        && let Ok(Some(status)) = a.child.try_wait()
    {
        tracing::info!(%status, "the agent exited while the link was down");
        *agent = None;
    }
    send(
        &mut sink,
        &FromSupervisor::Hello {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            agent: agent.as_ref().and_then(Agent::running),
            local_proxy: LOCAL_PROXY_UP.load(std::sync::atomic::Ordering::SeqCst),
            fetch_transcript: true,
        },
    )
    .await?;

    loop {
        let message = tokio::time::timeout(LINK_TIMEOUT, stream.next())
            .await
            .context("no launch or attach from the runtime")?;
        let Some(message) = message else {
            bail!("link closed before launch");
        };
        let Message::Text(text) = message? else {
            continue;
        };
        let message = match serde_json::from_str(&text) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!(error = %e, "bad message from runtime");
                continue;
            }
        };
        match message {
            ToSupervisor::Attach { runtime_build } => {
                if agent.is_none() {
                    bail!("the runtime attached, but no agent is running");
                }
                write_runtime_info(args, runtime_build.as_deref());
                tracing::info!("attached to the running agent");
                break;
            }
            ToSupervisor::Launch(launch) => {
                if let Some(mut old) = agent.take() {
                    tracing::info!("replacing the running agent");
                    stop(&mut old.child).await;
                }
                *agent = Some(launch_agent(args, &launch, &mut sink, &mut stream).await?);
                break;
            }
            _ => {}
        }
    }
    let a = agent.as_mut().expect("an agent is running");
    if let Some(message) = a.unsent.take() {
        if send(
            &mut sink,
            &FromSupervisor::Acp {
                message: message.clone(),
            },
        )
        .await
        .is_err()
        {
            a.unsent = Some(message);
            return Ok(Ending::Reconnect);
        }
        track(&mut a.waiting, &mut a.open, &message);
    }

    enum Event {
        LinkEnded,
        AgentExited,
        Terminate,
    }
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    // Results of commands the runtime asked to run, sent back on this link.
    let (results_tx, mut results) =
        tokio::sync::mpsc::unbounded_channel::<(u64, Result<String, String>)>();
    let deadline = tokio::time::sleep(LINK_TIMEOUT);
    tokio::pin!(deadline);
    let event = loop {
        tokio::select! {
            line = a.stdout.next_line() => match line {
                Ok(Some(line)) => {
                    if line.trim().is_empty() { continue; }
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        tracing::warn!(line = %line.chars().take(200).collect::<String>(), "non-JSON line from agent");
                        continue;
                    };
                    if send(&mut sink, &FromSupervisor::Acp { message: message.clone() }).await.is_err() {
                        a.unsent = Some(message);
                        break Event::LinkEnded;
                    }
                    track(&mut a.waiting, &mut a.open, &message);
                }
                Ok(None) | Err(_) => break Event::AgentExited,
            },
            message = stream.next() => {
                deadline.as_mut().reset(tokio::time::Instant::now() + LINK_TIMEOUT);
                heartbeat();
                match message {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<ToSupervisor>(&text) {
                        Ok(ToSupervisor::Acp { message }) => {
                            let mut line = serde_json::to_vec(&message)?;
                            line.push(b'\n');
                            // The agent's stdin only fails when the agent has gone.
                            if a.stdin.write_all(&line).await.is_err() || a.stdin.flush().await.is_err() {
                                break Event::AgentExited;
                            }
                            track(&mut a.open, &mut a.waiting, &message);
                        }
                        Ok(ToSupervisor::AgentReady(ready)) => a.ready = Some(ready),
                        Ok(ToSupervisor::SyncTranscript) => sync_transcript(&a.transcript_dir, &mut a.synced, &mut sink).await,
                        Ok(ToSupervisor::ResyncTranscript { name }) => {
                            a.synced.remove(&name);
                            sync_transcript(&a.transcript_dir, &mut a.synced, &mut sink).await;
                        }
                        Ok(ToSupervisor::Run { id, command, env, input, timeout_secs }) => {
                            let (base, cwd, results) = (a.env.clone(), a.cwd.clone(), results_tx.clone());
                            tokio::spawn(async move {
                                let result = run_command(
                                    &command,
                                    base.into_iter().chain(env),
                                    &cwd,
                                    &input,
                                    Duration::from_secs(timeout_secs),
                                )
                                .await;
                                let _ = results.send((id, result));
                            });
                        }
                        Ok(ToSupervisor::Launch(_) | ToSupervisor::Attach { .. }) => {}
                        Err(e) => tracing::warn!(error = %e, "bad message from runtime"),
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break Event::LinkEnded,
                    Some(Ok(_)) => {}
                }
            },
            Some((id, result)) = results.recv() => {
                let (output, error) = match result {
                    Ok(output) => (Some(output), None),
                    Err(error) => (None, Some(error)),
                };
                if send(&mut sink, &FromSupervisor::RunResult { id, output, error }).await.is_err() {
                    break Event::LinkEnded;
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break Event::LinkEnded;
                }
            }
            _ = &mut deadline => {
                tracing::warn!("no word from the runtime; dropping the link");
                break Event::LinkEnded;
            }
            _ = terminate.recv() => break Event::Terminate,
        }
    };
    let ending = match event {
        Event::LinkEnded => Ending::Reconnect,
        Event::AgentExited => {
            let mut a = agent.take().expect("an agent is running");
            // Its stdin or stdout closing usually means it exited; one that lingers on is stopped.
            let status = match tokio::time::timeout(Duration::from_secs(5), a.child.wait()).await {
                Ok(status) => status.ok(),
                Err(_) => {
                    stop(&mut a.child).await;
                    None
                }
            };
            let _ = send(
                &mut sink,
                &FromSupervisor::AgentExited {
                    code: status.and_then(|s| s.code()),
                },
            )
            .await;
            sync_transcript(&a.transcript_dir, &mut a.synced, &mut sink).await;
            Ending::Reconnect
        }
        Event::Terminate => {
            tracing::info!("terminating");
            let mut a = agent.take().expect("an agent is running");
            // Before the agent is stopped, which ends what it started.
            let running = a.child.id().map(shutdown::running).unwrap_or_default();
            if !running.is_empty() {
                let _ = send(&mut sink, &FromSupervisor::ShutdownReport { running }).await;
            }
            stop(&mut a.child).await;
            sync_transcript(&a.transcript_dir, &mut a.synced, &mut sink).await;
            Ending::Terminate
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), sink.close()).await;
    Ok(ending)
}

/// Prepares the pod and starts the agent; the link stays read meanwhile, so the runtime sees it's alive.
async fn launch_agent(
    args: &Args,
    launch: &Launch,
    sink: &mut Sink,
    stream: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) -> anyhow::Result<Agent> {
    let prepared = tokio::select! {
        prepared = prepare(args, launch, sink) => prepared,
        _ = closed(stream) => bail!("link closed while preparing the pod"),
    };
    let mut agent_env = Vec::new();
    let spawned = prepared.and_then(|env| {
        agent_env = env.clone();
        let [program, args @ ..] = &launch.command[..] else {
            bail!("the runtime sent no command to run");
        };
        Command::new(program)
            .args(args)
            .envs(env)
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {:?} in {}", launch.command, launch.cwd))
    });
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let _ = send(
                sink,
                &FromSupervisor::Error {
                    message: format!("preparing the pod: {e:#}"),
                },
            )
            .await;
            return Err(e);
        }
    };
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("agent stdout")?).lines();
    send(sink, &FromSupervisor::AgentStarted).await?;
    tracing::info!(session = %launch.session_id, "agent started");
    Ok(Agent {
        child,
        stdin,
        stdout,
        transcript_dir: launch.transcript_dir.clone(),
        ready: None,
        open: HashMap::new(),
        waiting: HashMap::new(),
        unsent: None,
        // The runtime has these already: what's synced from them is what's appended.
        synced: launch
            .transcript_files
            .iter()
            .map(|f| (f.name.clone(), f.len))
            .collect(),
        env: agent_env,
        cwd: launch.cwd.clone(),
    })
}

/// Runs a command the runtime asked for, apart from the agent; what it printed, or why it failed.
async fn run_command(
    command: &[String],
    env: impl IntoIterator<Item = (String, String)>,
    cwd: &str,
    input: &str,
    timeout: Duration,
) -> Result<String, String> {
    let [program, args @ ..] = command else {
        return Err("no command to run".into());
    };
    let mut child = Command::new(program)
        .args(args)
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("starting {program}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    stdin
        .write_all(input.as_bytes())
        .await
        .map_err(|e| format!("writing its input: {e}"))?;
    drop(stdin);
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("it took longer than {}s", timeout.as_secs()))?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "it exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

/// Fetches a transcript file from the runtime into `path`, through a partial file so a fetch cut off midway isn't
/// taken for the whole file next time.
/// Where the runtime serves a transcript file; the name is one path segment, whatever it contains.
fn transcript_url(base: &str, name: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("the runtime URL {base} can't have a path"))?
        .pop_if_empty()
        .extend(
            sloppod_protocol::link::TRANSCRIPT_PATH
                .split('/')
                .filter(|s| !s.is_empty()),
        )
        .push(name);
    Ok(url)
}

async fn fetch_transcript(args: &Args, name: &str, path: &Path) -> anyhow::Result<()> {
    let token = std::fs::read_to_string(&args.token_file).context("reading the pod's token")?;
    let mut response = reqwest::Client::new()
        .get(transcript_url(&args.url, name)?)
        .bearer_auth(token.trim())
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("fetching the transcript file {name}"))?;
    let partial = path.with_extension("jsonl.partial");
    let mut file = tokio::fs::File::create(&partial).await?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&partial, path).await?;
    Ok(())
}

/// Where to sync a transcript file of `len` bytes from, given how much the runtime has; `None` when it's up to date.
/// A file that shrank (or one the runtime has none of) is sent whole.
fn sync_from(len: u64, synced: Option<u64>) -> Option<u64> {
    match synced {
        Some(have) if have == len => None,
        Some(have) if have < len => Some(have),
        _ => Some(0),
    }
}

/// Sends the runtime what was appended to each transcript file since the last sync (ADR 1). It stops at the first
/// send that fails, since the link is gone then.
/// The most of a transcript file one sync message carries.
const SYNC_CHUNK: usize = 4 << 20;

/// What's to sync, in pieces, so a file sent whole isn't one message the size of the file. Nothing to send is still
/// one piece: a file that became empty replaces the runtime's copy with nothing.
fn sync_pieces(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return vec![content];
    }
    content.chunks(SYNC_CHUNK).collect()
}

async fn sync_transcript(dir: &str, synced: &mut HashMap<String, u64>, sink: &mut Sink) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only names the runtime stores and a later pod restores.
        if !sloppod_protocol::link::is_transcript_file_name(&name) {
            continue;
        }
        let Ok(appended) = read_appended(&entry.path(), synced.get(&name).copied()).await else {
            continue;
        };
        let Some((offset, content)) = appended else {
            continue;
        };
        let pieces = sync_pieces(&content);
        // A replacement counts as synced only once all of it is sent; one cut off is sent whole again.
        let replacing = offset == 0 && pieces.len() > 1;
        let mut sent = offset;
        for piece in pieces {
            let message = FromSupervisor::TranscriptAppend {
                name: name.clone(),
                offset: sent,
                content_b64: base64::engine::general_purpose::STANDARD.encode(piece),
                replaces: (replacing && sent == 0).then_some(content.len() as u64),
            };
            if let Err(e) = send(sink, &message).await {
                tracing::warn!(error = %e, file = %name, "syncing the transcript failed");
                return;
            }
            sent += piece.len() as u64;
            if !replacing {
                synced.insert(name.clone(), sent);
            }
        }
        synced.insert(name, sent);
    }
}

/// What was appended to `path` since `synced` bytes of it were sent, with the offset it starts at; `None` if
/// nothing was. Only that part is read.
async fn read_appended(
    path: &Path,
    synced: Option<u64>,
) -> std::io::Result<Option<(u64, Vec<u8>)>> {
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let Some(offset) = sync_from(len, synced) else {
        return Ok(None);
    };
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut content = Vec::new();
    file.read_to_end(&mut content).await?;
    Ok(Some((offset, content)))
}

/// Tells the agent which runtime build it's connected to. It's informational, so a failure doesn't stop anything.
fn write_runtime_info(args: &Args, build: Option<&str>) {
    let content = serde_json::to_vec_pretty(&serde_json::json!({ "build": build }))
        .expect("a JSON object serializes");
    if let Err(e) = write_file(&args.workdir.join(RUNTIME_INFO), &content) {
        tracing::warn!(error = %format!("{e:#}"), "writing the runtime info");
    }
}

fn write_file(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// gh extensions that come with the image; gh only finds extensions in its data directory, which is on the volume.
const GH_EXTENSIONS: &[&str] = &["/usr/local/bin/gh-stack"];

fn link_gh_extensions(home: &Path, extensions: &[&str]) -> anyhow::Result<()> {
    for binary in extensions.iter().map(Path::new).filter(|b| b.exists()) {
        let Some(name) = binary.file_name() else {
            continue;
        };
        let dir = home.join(".local/share/gh/extensions").join(name);
        let link = dir.join(name);
        if link.symlink_metadata().is_err() {
            std::fs::create_dir_all(&dir)?;
            std::os::unix::fs::symlink(binary, &link)?;
        }
    }
    Ok(())
}

/// Prepares the pod for the agent and returns its environment.
async fn prepare(
    args: &Args,
    launch: &Launch,
    sink: &mut Sink,
) -> anyhow::Result<Vec<(String, String)>> {
    let home =
        std::env::var("HOME").unwrap_or_else(|_| args.workdir.join("home").display().to_string());
    std::fs::create_dir_all(&home)?;
    if let Ok(tmp) = std::env::var("TMPDIR") {
        std::fs::create_dir_all(tmp)?;
    }
    link_gh_extensions(Path::new(&home), GH_EXTENSIONS)?;
    let mut env: Vec<(String, String)> = Vec::new();
    let path = std::env::var("PATH").unwrap_or_default();
    env.push((
        "PATH".into(),
        format!("{home}/.local/share/mise/shims:{home}/.local/bin:{path}"),
    ));
    env.push(("MISE_YES".into(), "1".into()));
    env.push(("GH_STACK_NO_UPDATE_NOTIFIER".into(), "1".into()));
    env.push(("GIT_CONFIG_COUNT".into(), "2".into()));
    env.push(("GIT_CONFIG_KEY_0".into(), "credential.helper".into()));
    env.push((
        "GIT_CONFIG_VALUE_0".into(),
        "!f() { echo username=x-access-token; echo password=sloppod-placeholder; }; f".into(),
    ));
    env.push(("GIT_CONFIG_KEY_1".into(), "push.default".into()));
    env.push(("GIT_CONFIG_VALUE_1".into(), "current".into()));

    write_runtime_info(args, launch.runtime_build.as_deref());

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
        if !sloppod_protocol::link::is_transcript_file_name(&file.name) {
            tracing::warn!(file = %file.name, "not restoring a transcript file with that name");
            continue;
        }
        let path = Path::new(&launch.transcript_dir).join(&file.name);
        if !path.exists() {
            let content = base64::engine::general_purpose::STANDARD.decode(&file.content_b64)?;
            write_file(&path, &content)?;
            tracing::info!(file = %file.name, "restored transcript");
        }
    }

    for TranscriptRef { name, .. } in &launch.transcript_files {
        if !sloppod_protocol::link::is_transcript_file_name(name) {
            tracing::warn!(file = %name, "not restoring a transcript file with that name");
            continue;
        }
        let path = Path::new(&launch.transcript_dir).join(name);
        if !path.exists() {
            fetch_transcript(args, name, &path).await?;
            tracing::info!(file = %name, "restored transcript");
        }
    }

    if let Some(repo) = &launch.repo {
        let dir = Path::new(&repo.path);
        if !dir.join(".git").exists() {
            let _ = send(
                sink,
                &FromSupervisor::Status {
                    message: format!("cloning {}", repo.clone_url),
                },
            )
            .await;
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
        if let Some(commit) = &repo.commit {
            // The commit itself, which the branch may have moved away from since; the branch if the remote won't
            // serve a commit by name.
            if run_cmd(&env, &repo.path, "git", &["fetch", "origin", commit])
                .await
                .is_err()
            {
                let remote_branch = format!("refs/heads/{}", repo.branch);
                run_cmd(
                    &env,
                    &repo.path,
                    "git",
                    &["fetch", "origin", &remote_branch],
                )
                .await?;
            }
            run_cmd(&env, &repo.path, "git", &["checkout", "--detach", commit]).await?;
        }
        if launch.install_tools && has_mise_config(dir) {
            let _ = send(
                sink,
                &FromSupervisor::Status {
                    message: "installing tools with mise".into(),
                },
            )
            .await;
            // Only locked versions, verified against the lockfile's checksums and provenance (ADR 12). Only for this
            // install: the agent may still add tools and lock them.
            let mut install_env = env.clone();
            install_env.push(("MISE_LOCKED".into(), "1".into()));
            install_env.push(("MISE_LOCKED_VERIFY_PROVENANCE".into(), "1".into()));
            let result = if dir.join("mise.lock").exists() {
                async {
                    run_cmd(&env, &repo.path, "mise", &["trust", "--yes", "--all"]).await?;
                    run_cmd(&install_env, &repo.path, "mise", &["install"]).await
                }
                .await
            } else {
                Err(anyhow::anyhow!(
                    "the repository has no mise.lock, so its tools weren't installed; add one with `mise lock`"
                ))
            };
            if let Err(e) = result {
                let _ = send(
                    sink,
                    &FromSupervisor::Status {
                        message: format!("mise install failed: {e:#}"),
                    },
                )
                .await;
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_sync_goes_in_bounded_pieces_and_an_empty_file_still_syncs() {
        let big = vec![b'x'; SYNC_CHUNK * 2 + 1];
        let lens: Vec<usize> = sync_pieces(&big).iter().map(|p| p.len()).collect();
        assert_eq!(lens, [SYNC_CHUNK, SYNC_CHUNK, 1]);
        assert_eq!(sync_pieces(b""), [b"" as &[u8]]);
    }

    #[test]
    fn a_transcript_name_is_one_path_segment() {
        for (name, path) in [
            ("a.jsonl", "/link/v1/transcript/a.jsonl"),
            ("a#b.jsonl", "/link/v1/transcript/a%23b.jsonl"),
            ("a?b.jsonl", "/link/v1/transcript/a%3Fb.jsonl"),
            ("a%b.jsonl", "/link/v1/transcript/a%25b.jsonl"),
        ] {
            let url = transcript_url("http://sloppod:8080/", name).unwrap();
            assert_eq!(url.path(), path, "{name}");
            assert_eq!((url.query(), url.fragment()), (None, None));
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn requests_stay_open_until_answered() {
        let (mut open, mut waiting) = (HashMap::new(), HashMap::new());
        let prompt = json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {}});
        track(&mut open, &mut waiting, &prompt);
        let question =
            json!({"jsonrpc": "2.0", "id": 7, "method": "elicitation/create", "params": {}});
        track(&mut waiting, &mut open, &question);
        track(
            &mut waiting,
            &mut open,
            &json!({"jsonrpc": "2.0", "method": "session/update", "params": {}}),
        );
        assert_eq!(open.values().collect::<Vec<_>>(), [&prompt]);
        assert_eq!(waiting.values().collect::<Vec<_>>(), [&question]);

        // The runtime answers the question; the agent's id 7 is a different request from the runtime's.
        track(
            &mut open,
            &mut waiting,
            &json!({"jsonrpc": "2.0", "id": 7, "result": {}}),
        );
        assert!(waiting.is_empty());
        assert_eq!(open.len(), 1);
        track(
            &mut waiting,
            &mut open,
            &json!({"jsonrpc": "2.0", "id": 7, "result": {}}),
        );
        assert!(open.is_empty());
    }
}

#[cfg(test)]
mod gh_extension_tests {
    use super::*;

    #[test]
    fn links_extensions_that_exist() {
        let root = std::env::temp_dir().join(format!("gh-ext-{}", std::process::id()));
        let binary = root.join("gh-stack");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&binary, "").unwrap();
        let home = root.join("home");
        let extensions = [binary.to_str().unwrap(), "/nonexistent/gh-other"];

        link_gh_extensions(&home, &extensions).unwrap();
        // A second start finds the link already there.
        link_gh_extensions(&home, &extensions).unwrap();

        let link = home.join(".local/share/gh/extensions/gh-stack/gh-stack");
        assert_eq!(std::fs::read_link(&link).unwrap(), binary);
        assert!(!home.join(".local/share/gh/extensions/gh-other").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[cfg(test)]
mod sync_tests {
    use super::{read_appended, sync_from};

    #[test]
    fn syncs_only_what_was_appended() {
        assert_eq!(sync_from(100, None), Some(0));
        assert_eq!(sync_from(100, Some(100)), None);
        assert_eq!(sync_from(150, Some(100)), Some(100));
        assert_eq!(sync_from(80, Some(100)), Some(0));
        assert_eq!(sync_from(0, Some(0)), None);
    }

    #[tokio::test]
    async fn reads_only_what_was_appended() {
        let path = std::env::temp_dir().join(format!("sloppod-sync-{}.jsonl", std::process::id()));
        std::fs::write(&path, b"one\ntwo\n").unwrap();
        assert_eq!(
            read_appended(&path, None).await.unwrap(),
            Some((0, b"one\ntwo\n".to_vec()))
        );
        assert_eq!(
            read_appended(&path, Some(4)).await.unwrap(),
            Some((4, b"two\n".to_vec()))
        );
        assert_eq!(read_appended(&path, Some(8)).await.unwrap(), None);
        assert_eq!(
            read_appended(&path, Some(20)).await.unwrap(),
            Some((0, b"one\ntwo\n".to_vec()))
        );
        std::fs::remove_file(&path).unwrap();
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[tokio::test]
    async fn a_command_gets_its_input_and_the_agents_environment() {
        let env = [
            ("JUDGE".to_owned(), "base".to_owned()),
            ("JUDGE".to_owned(), "changed".to_owned()),
        ];
        let out = run_command(
            &sh("cat; echo \" $JUDGE\""),
            env,
            "/",
            "hello",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(out, "hello changed\n");
    }

    #[tokio::test]
    async fn a_failure_or_a_timeout_says_why() {
        let failed = run_command(
            &sh("echo oops >&2; exit 3"),
            [],
            "/",
            "",
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(failed.contains("oops"), "{failed}");
        let slow = run_command(&sh("sleep 5"), [], "/", "", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(slow.contains("longer than"), "{slow}");
        assert!(
            run_command(&[], [], "/", "", Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
