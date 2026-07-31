#![forbid(unsafe_code)]
#![doc = "Command-line entry point for acp-tunnel."]

use std::{env, net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use acp_tunnel::{
    Error, Result,
    auth::StaticTokenAuthenticator,
    client::{ConnectOptions, connect},
    config::ServerConfig,
    server::{ServerState, serve},
};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "acp-tunnel",
    version,
    about = "Tunnel stdio ACP agents over authenticated WebSockets"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Impersonate a local ACP agent and connect to a remote tunnel server.
    Connect {
        /// Authenticated WebSocket endpoint.
        #[arg(long)]
        url: Url,
        /// Server-configured agent identifier.
        #[arg(long)]
        agent: String,
        /// Server-configured workspace identifier.
        #[arg(long)]
        workspace: String,
        /// Maximum ACP line and WebSocket message size.
        #[arg(long, default_value_t = 10 * 1024 * 1024)]
        max_frame_bytes: usize,
        /// Maximum unacknowledged ACP frames retained for replay.
        #[arg(long, default_value_t = 256)]
        max_replay_frames: usize,
        /// Maximum unacknowledged ACP payload bytes retained for replay.
        #[arg(long, default_value_t = 20 * 1024 * 1024)]
        max_replay_bytes: usize,
        /// Connection and opening timeout in seconds.
        #[arg(long, default_value_t = 10)]
        connect_timeout_seconds: u64,
        /// Time without server traffic before failing.
        #[arg(long, default_value_t = 45)]
        keepalive_timeout_seconds: u64,
        /// Maximum time spent reconnecting a detached tunnel.
        #[arg(long, default_value_t = 30)]
        reconnect_timeout_seconds: u64,
    },
    /// Serve configured ACP agents over authenticated WebSockets.
    Serve {
        /// TOML server configuration.
        #[arg(long)]
        config: PathBuf,
        /// Override the configured listener address.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Permit plaintext HTTP on a non-loopback listener.
        #[arg(long)]
        insecure_listen: bool,
    },
    /// Parse and validate a server configuration without starting a server.
    CheckConfig {
        /// TOML server configuration.
        #[arg(long)]
        config: PathBuf,
    },
    /// Internal infrastructure-free integration-test ACP agent.
    #[command(name = "__test-agent", hide = true)]
    TestAgent,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("acp-tunnel: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Connect {
            url,
            agent,
            workspace,
            max_frame_bytes,
            max_replay_frames,
            max_replay_bytes,
            connect_timeout_seconds,
            keepalive_timeout_seconds,
            reconnect_timeout_seconds,
        } => {
            let token = required_token()?;
            connect(ConnectOptions {
                url,
                agent,
                workspace,
                token,
                max_frame_bytes,
                max_replay_frames,
                max_replay_bytes,
                connection_timeout: Duration::from_secs(connect_timeout_seconds),
                keepalive_timeout: Duration::from_secs(keepalive_timeout_seconds),
                reconnect_timeout: Duration::from_secs(reconnect_timeout_seconds),
            })
            .await
        }
        Command::Serve {
            config,
            listen,
            insecure_listen,
        } => {
            init_server_logging()?;
            let mut config = ServerConfig::load(config)?;
            if let Some(listen) = listen {
                config.listen = listen;
            }
            config.validate()?;
            let token = required_token()?;
            let shutdown = CancellationToken::new();
            let signal_shutdown = shutdown.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal_shutdown.cancel();
                }
            });
            let state = ServerState::new(
                Arc::new(config),
                Arc::new(StaticTokenAuthenticator::new(token)),
                shutdown.clone(),
            );
            serve(state, insecure_listen, shutdown).await
        }
        Command::CheckConfig { config } => {
            let loaded = ServerConfig::load(&config)?;
            println!(
                "configuration is valid: {} agent(s), {} workspace(s), {} MCP server(s)",
                loaded.agents.len(),
                loaded.workspaces.len(),
                loaded.mcp_servers.len()
            );
            Ok(())
        }
        Command::TestAgent => run_test_agent().await,
    }
}

fn required_token() -> Result<String> {
    let token = env::var("ACP_TUNNEL_TOKEN").map_err(|_| {
        Error::Config("ACP_TUNNEL_TOKEN must be set and is never accepted as a CLI argument".into())
    })?;
    if token.is_empty() {
        Err(Error::Config("ACP_TUNNEL_TOKEN must not be empty".into()))
    } else {
        Ok(token)
    }
}

fn init_server_logging() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| Error::Config(format!("cannot initialize structured logging: {error}")))
}

async fn run_test_agent() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = BufWriter::new(tokio::io::stdout());
    eprintln!("fake-agent pid={}", std::process::id());
    while let Some(line) = lines.next_line().await? {
        let request: Value = serde_json::from_str(&line)?;
        let method = request.get("method").and_then(Value::as_str);
        let id = request.get("id").cloned();
        if method.is_none() {
            continue;
        }
        match method {
            Some("initialize") => {
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{},
                            "agentInfo":{"name":"acp-tunnel-test-agent","version":"0"}
                        }
                    }),
                )
                .await?;
            }
            Some("session/new") => {
                let observed_cwd = request
                    .pointer("/params/cwd")
                    .cloned()
                    .unwrap_or(Value::Null);
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "result":{"sessionId":"test-session","observedCwd":observed_cwd}
                    }),
                )
                .await?;
            }
            Some("session/prompt") => {
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "method":"session/update",
                        "params":{
                            "sessionId":"test-session",
                            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"test"}}
                        }
                    }),
                )
                .await?;
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":"agent-permission-1",
                        "method":"session/request_permission",
                        "params":{"sessionId":"test-session","options":[]}
                    }),
                )
                .await?;
                write_json(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                )
                .await?;
            }
            Some("session/cancel") => {
                if id.is_some() {
                    write_json(&mut stdout, &json!({"jsonrpc":"2.0","id":id,"result":{}})).await?;
                }
            }
            Some("test/exit") => {
                write_json(&mut stdout, &json!({"jsonrpc":"2.0","id":id,"result":{}})).await?;
                break;
            }
            Some("test/stderr") => {
                for index in 0..1_000 {
                    eprintln!("noisy diagnostic {index}");
                }
                write_json(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":id,"result":{"stderrComplete":true}}),
                )
                .await?;
            }
            Some("test/pid") => {
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "result":{"pid":std::process::id()}
                    }),
                )
                .await?;
            }
            _ if id.is_some() => {
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "error":{"code":-32601,"message":"method not found"}
                    }),
                )
                .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn write_json(output: &mut BufWriter<tokio::io::Stdout>, value: &Value) -> Result<()> {
    let encoded = serde_json::to_vec(value)?;
    output.write_all(&encoded).await?;
    output.write_all(b"\n").await?;
    output.flush().await?;
    Ok(())
}
