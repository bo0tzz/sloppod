//! `sloppodctl`: a small client for the runtime's REST API and `/acp`, for testing and scripting.

use std::io::Write;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sloppod_protocol::api::{NewSession, Proposal, Session};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The runtime's base URL.
    #[arg(long, env = "SLOPPOD_URL", default_value = "http://localhost:8080")]
    url: String,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List sessions.
    Sessions,
    /// Create a session.
    /// Start a session from its first message; the repository is derived from it unless given.
    Create {
        prompt: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// The agent to run, e.g. `codex`; Claude Code by default.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Send a prompt and stream the turn.
    Prompt {
        session: Uuid,
        text: String,
        /// Answer permission requests with the first "allow" option, and questions with each one's first option.
        #[arg(long)]
        allow: bool,
    },
    /// Steer a running turn.
    Steer { session: Uuid, text: String },
    /// Print a session's history.
    Log { session: Uuid },
    /// Wait until the session's agent is idle.
    Wait {
        session: Uuid,
        #[arg(long, default_value_t = 900)]
        timeout: u64,
    },
    /// Stop a session and delete its pod.
    Stop { session: Uuid },
    /// List proposals.
    Proposals {
        #[arg(long)]
        session: Option<Uuid>,
    },
    /// Approve a proposal, executing it.
    Approve { proposal: Uuid },
    /// Reject a proposal.
    Reject {
        proposal: Uuid,
        #[arg(long)]
        reason: Option<String>,
    },
    /// List credentials.
    Credentials,
    /// Add a credential; the secret is read from a file, or stdin with `-`.
    AddCredential {
        /// claude, github-app, memini, chatgpt or github-user
        #[arg(long)]
        kind: String,
        #[arg(long)]
        name: String,
        /// Non-secret config as key=value, e.g. app_id=123 or url=https://memini.example
        #[arg(long = "config", value_parser = parse_key_value)]
        config: Vec<(String, String)>,
        #[arg(long)]
        secret_file: String,
    },
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| format!("expected key=value, got {s}"))
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn check(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "{status}: {}",
            body["error"].as_str().unwrap_or("request failed")
        );
    }
    Ok(body)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("installing the rustls crypto provider"))?;
    let args = Args::parse();
    let api = format!("{}/api/v1", args.url.trim_end_matches('/'));
    match args.command {
        Cmd::Sessions => {
            let sessions: Vec<Session> = serde_json::from_value(
                check(http().get(format!("{api}/sessions")).send().await?).await?,
            )?;
            for s in sessions {
                println!(
                    "{}  {:<12}  {:<24}  {}",
                    s.id,
                    s.phase.as_str(),
                    s.repo.as_deref().unwrap_or("-"),
                    s.title
                );
            }
        }
        Cmd::Create {
            repo,
            title,
            prompt,
            agent,
        } => {
            let body = NewSession {
                repo: repo.map(Some),
                title,
                prompt,
                agent,
                capabilities: None,
                model: None,
            };
            let session: Session = serde_json::from_value(
                check(
                    http()
                        .post(format!("{api}/sessions"))
                        .json(&body)
                        .send()
                        .await?,
                )
                .await?,
            )?;
            println!("{}", session.id);
        }
        Cmd::Prompt {
            session,
            text,
            allow,
        } => {
            acp(
                &args.url,
                session,
                Some(("session/prompt", text)),
                allow,
                false,
            )
            .await?;
        }
        Cmd::Steer { session, text } => {
            acp(
                &args.url,
                session,
                Some(("_session/steering", text)),
                false,
                false,
            )
            .await?;
        }
        Cmd::Log { session } => {
            acp(&args.url, session, None, false, true).await?;
        }
        Cmd::Wait { session, timeout } => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                let s: Session = serde_json::from_value(
                    check(
                        http()
                            .get(format!("{api}/sessions/{session}"))
                            .send()
                            .await?,
                    )
                    .await?,
                )?;
                match s.phase.as_str() {
                    "idle" => return Ok(()),
                    "stopped" => bail!("session stopped"),
                    _ => {}
                }
                if std::time::Instant::now() > deadline {
                    bail!("timed out in phase {}", s.phase.as_str());
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
        Cmd::Stop { session } => {
            let response = http()
                .delete(format!("{api}/sessions/{session}"))
                .send()
                .await?;
            if !response.status().is_success() {
                bail!("{}", response.status());
            }
        }
        Cmd::Proposals { session } => {
            let mut url = format!("{api}/proposals");
            if let Some(session) = session {
                url = format!("{url}?session={session}");
            }
            let proposals: Vec<Proposal> =
                serde_json::from_value(check(http().get(url).send().await?).await?)?;
            for p in proposals {
                println!(
                    "{}  {:<9}  {}  {}",
                    p.id,
                    p.status,
                    p.kind,
                    p.payload["title"].as_str().unwrap_or_default()
                );
            }
        }
        Cmd::Approve { proposal } => {
            let result = check(
                http()
                    .post(format!("{api}/proposals/{proposal}/approve"))
                    .json(&json!({}))
                    .send()
                    .await?,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Reject { proposal, reason } => {
            let result = check(
                http()
                    .post(format!("{api}/proposals/{proposal}/reject"))
                    .json(&json!({"reason": reason}))
                    .send()
                    .await?,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Credentials => {
            let credentials = check(http().get(format!("{api}/credentials")).send().await?).await?;
            for c in credentials.as_array().into_iter().flatten() {
                println!(
                    "{}  {:<10}  {:<20}  {}{}",
                    c["id"].as_str().unwrap_or_default(),
                    c["kind"].as_str().unwrap_or_default(),
                    c["name"].as_str().unwrap_or_default(),
                    c["config"],
                    if c["default"].as_bool() == Some(true) {
                        "  (default)"
                    } else {
                        ""
                    }
                );
            }
        }
        Cmd::AddCredential {
            kind,
            name,
            config,
            secret_file,
        } => {
            let secret = if secret_file == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(&secret_file)?
            };
            let config: serde_json::Map<String, Value> = config
                .into_iter()
                .map(|(k, v)| (k, Value::from(v)))
                .collect();
            let body = json!({"kind": kind, "name": name, "config": config, "secret": secret});
            let created = check(
                http()
                    .post(format!("{api}/credentials"))
                    .json(&body)
                    .send()
                    .await?,
            )
            .await?;
            println!("{}", created["id"].as_str().unwrap_or_default());
        }
    }
    Ok(())
}

fn print_update(update: &Value) {
    let mut out = std::io::stdout();
    match update["sessionUpdate"].as_str().unwrap_or_default() {
        "agent_message_chunk" => {
            let _ = write!(
                out,
                "{}",
                update["content"]["text"].as_str().unwrap_or_default()
            );
        }
        "user_message_chunk" => {
            let _ = writeln!(
                out,
                "\n> {}",
                update["content"]["text"].as_str().unwrap_or_default()
            );
        }
        "tool_call" => {
            let _ = writeln!(
                out,
                "\n[tool] {}",
                update["title"].as_str().unwrap_or_default()
            );
        }
        "tool_call_update" => {
            if let Some(status) = update["status"].as_str() {
                let _ = writeln!(
                    out,
                    "[tool {}] {}",
                    status,
                    update["title"].as_str().unwrap_or_default()
                );
            }
        }
        _ => {}
    }
    let _ = out.flush();
}

async fn acp(
    base: &str,
    session: Uuid,
    send: Option<(&str, String)>,
    allow: bool,
    replay: bool,
) -> anyhow::Result<()> {
    let url = format!(
        "{}/acp",
        base.trim_end_matches('/').replacen("http", "ws", 1)
    );
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .context("connecting to /acp")?;
    let (mut sink, mut stream) = socket.split();
    let mut send_message =
        async |message: Value| sink.send(Message::Text(message.to_string().into())).await;
    send_message(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})).await?;
    send_message(json!({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": session, "cwd": "/", "mcpServers": []}})).await?;
    let mut loaded = false;
    while let Some(message) = stream.next().await {
        let Message::Text(text) = message? else {
            continue;
        };
        let message: Value = serde_json::from_str(&text)?;
        match (message["method"].as_str(), message["id"].as_i64()) {
            (Some("session/update"), _) if loaded || replay => {
                print_update(&message["params"]["update"])
            }
            (Some("session/request_permission"), Some(id)) => {
                let params = &message["params"];
                println!(
                    "\n[permission] {}",
                    params["toolCall"]["title"].as_str().unwrap_or_default()
                );
                if allow {
                    let options = params["options"].as_array().cloned().unwrap_or_default();
                    let pick = options
                        .iter()
                        .find(|o| o["kind"] == "allow_once")
                        .or(options.first())
                        .context("permission request without options")?;
                    send_message(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": pick["optionId"]}}})).await?;
                }
            }
            (Some("elicitation/create"), Some(id)) => {
                let params = &message["params"];
                println!(
                    "\n[question] {}",
                    params["message"].as_str().unwrap_or_default()
                );
                let mut content = serde_json::Map::new();
                let fields = params["requestedSchema"]["properties"].as_object();
                for (key, field) in fields.into_iter().flatten() {
                    let options = field["oneOf"]
                        .as_array()
                        .or(field["items"]["anyOf"].as_array());
                    let Some(options) = options else { continue };
                    let titles: Vec<&str> =
                        options.iter().filter_map(|o| o["title"].as_str()).collect();
                    println!(
                        "  {}: {}",
                        field["title"].as_str().unwrap_or(key),
                        titles.join(" | ")
                    );
                    if let Some(first) = options.first() {
                        let pick = first["const"].clone();
                        content.insert(
                            key.clone(),
                            if field["type"] == "array" {
                                json!([pick])
                            } else {
                                pick
                            },
                        );
                    }
                }
                if allow {
                    println!("  answering: {}", Value::Object(content.clone()));
                    send_message(json!({"jsonrpc": "2.0", "id": id, "result": {"action": "accept", "content": content}})).await?;
                }
            }
            (Some("_sloppod/proposal"), _) => {
                let p = &message["params"]["proposal"];
                println!(
                    "\n[proposal {}] {} {}",
                    p["status"].as_str().unwrap_or_default(),
                    p["id"].as_str().unwrap_or_default(),
                    p["payload"]["title"].as_str().unwrap_or_default()
                );
            }
            (None, Some(2)) => {
                if let Some(error) = message.get("error") {
                    bail!("session/load: {error}");
                }
                loaded = true;
                let Some((method, text)) = &send else {
                    return Ok(());
                };
                send_message(json!({"jsonrpc": "2.0", "id": 3, "method": method, "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]}})).await?;
            }
            (None, Some(3)) => {
                println!();
                if let Some(error) = message.get("error") {
                    bail!("{error}");
                }
                if send.as_ref().is_some_and(|(m, _)| *m == "session/prompt") {
                    println!(
                        "[{}]",
                        message["result"]["stopReason"].as_str().unwrap_or("done")
                    );
                }
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}
