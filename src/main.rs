#![forbid(unsafe_code)]
#![doc = "Command-line entry point for acp-tunnel."]

use std::{
    collections::BTreeSet,
    io::{IsTerminal, Write as _},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use acp_tunnel::{
    Error, Result,
    auth::StaticTokenAuthenticator,
    client::{
        ConnectOptions, ShutdownHandle, connect_with_shutdown, select_client_environment,
        shutdown_channel,
    },
    config::{McpPolicy, ServerConfig},
    credentials::load_token,
    paths::{default_token_file, resolve_server_config_file},
    protocol::ShutdownReason,
    server::{ServerState, serve},
    setup::{DoctorLevel, InitOptions, diagnose_server, generate_user_service, initialize_server},
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use url::Url;

const BUZZ_CLIENT_ENVIRONMENT: [&str; 3] = ["BUZZ_RELAY_URL", "BUZZ_PRIVATE_KEY", "BUZZ_AUTH_TAG"];

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
        /// Maximum time to wait for remote shutdown confirmation.
        #[arg(long, default_value_t = 10)]
        shutdown_timeout_seconds: u64,
        /// Override the default bearer credential file.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Send one named local variable when the selected agent allowlists it.
        #[arg(long = "client-env", value_name = "NAME")]
        client_env: Vec<String>,
        /// Send the required Buzz session environment variables.
        #[arg(long)]
        buzz: bool,
    },
    /// Serve configured ACP agents over authenticated WebSockets.
    Serve {
        /// Override the default server configuration file.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the configured listener address.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Permit plaintext HTTP on a non-loopback listener.
        #[arg(long)]
        insecure_listen: bool,
        /// Override the default bearer credential file.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Parse and validate a server configuration without starting a server.
    CheckConfig {
        /// Override the default server configuration file.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Create a server configuration and bearer token.
    Init {
        /// Override the default server configuration file.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the default bearer credential file.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Public agent identifier.
        #[arg(long)]
        agent: Option<String>,
        /// Agent executable path or name.
        #[arg(long = "agent-command")]
        agent_command: Option<PathBuf>,
        /// Public workspace identifier.
        #[arg(long)]
        workspace: Option<String>,
        /// Existing remote workspace directory.
        #[arg(long)]
        workspace_path: Option<PathBuf>,
        /// Inherit one server variable. Defaults to HOME and PATH.
        #[arg(long = "pass-env", value_name = "NAME")]
        pass_env: Vec<String>,
        /// Accept the required Buzz session environment variables.
        #[arg(long)]
        buzz: bool,
        /// MCP policy. Initialization defaults to passthrough.
        #[arg(long, value_enum)]
        mcp_policy: Option<InitMcpPolicy>,
        /// Replace an existing server configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Diagnose the local server configuration and network setup.
    Doctor {
        /// Override the default server configuration file.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the default bearer credential file.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Public tunnel URL to resolve and connect to.
        #[arg(long)]
        url: Option<Url>,
    },
    /// Generate service-manager configuration.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Internal infrastructure-free integration-test ACP agent.
    #[command(name = "__test-agent", hide = true)]
    TestAgent {
        /// Ignore SIGTERM and remain alive after stdin closes.
        #[arg(long, hide = true)]
        uncooperative: bool,
        /// Start an uncooperative process in the same process group.
        #[arg(long, hide = true)]
        spawn_grandchild: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum InitMcpPolicy {
    Deny,
    Allowlisted,
    Passthrough,
}

impl From<InitMcpPolicy> for McpPolicy {
    fn from(value: InitMcpPolicy) -> Self {
        match value {
            InitMcpPolicy::Deny => Self::Deny,
            InitMcpPolicy::Allowlisted => Self::Allowlisted,
            InitMcpPolicy::Passthrough => Self::Passthrough,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Write a systemd user-service unit to stdout.
    Generate {
        /// Generate a unit for the current user's service manager.
        #[arg(long)]
        user: bool,
    },
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
            shutdown_timeout_seconds,
            token_file,
            mut client_env,
            buzz,
        } => {
            let token = load_token(token_file.as_deref())?;
            if buzz {
                client_env.extend(BUZZ_CLIENT_ENVIRONMENT.map(str::to_owned));
            }
            let client_environment = select_client_environment(&client_env)?;
            let (shutdown_handle, shutdown_signal) = shutdown_channel();
            install_connector_signal_handler(shutdown_handle)?;
            connect_with_shutdown(
                ConnectOptions {
                    url,
                    agent,
                    workspace,
                    token,
                    client_environment,
                    max_frame_bytes,
                    max_replay_frames,
                    max_replay_bytes,
                    connection_timeout: Duration::from_secs(connect_timeout_seconds),
                    keepalive_timeout: Duration::from_secs(keepalive_timeout_seconds),
                    reconnect_timeout: Duration::from_secs(reconnect_timeout_seconds),
                    shutdown_timeout: Duration::from_secs(shutdown_timeout_seconds),
                },
                shutdown_signal,
            )
            .await
        }
        Command::Serve {
            config,
            listen,
            insecure_listen,
            token_file,
        } => {
            init_server_logging()?;
            let config = resolve_server_config_file(config)?;
            let mut config = ServerConfig::load(config)?;
            if let Some(listen) = listen {
                config.listen = listen;
            }
            config.validate()?;
            let token = load_token(token_file.as_deref())?;
            let shutdown = CancellationToken::new();
            install_server_signal_handler(shutdown.clone())?;
            let state = ServerState::new(
                Arc::new(config),
                Arc::new(StaticTokenAuthenticator::new(token)),
                shutdown.clone(),
            );
            serve(state, insecure_listen, shutdown).await
        }
        Command::CheckConfig { config } => {
            let config = resolve_server_config_file(config)?;
            let loaded = ServerConfig::load(&config)?;
            println!(
                "configuration is valid: {} agent(s), {} workspace(s), {} MCP server(s)",
                loaded.agents.len(),
                loaded.workspaces.len(),
                loaded.mcp_servers.len()
            );
            Ok(())
        }
        Command::Init {
            config,
            token_file,
            agent,
            agent_command,
            workspace,
            workspace_path,
            pass_env,
            buzz,
            mcp_policy,
            force,
        } => run_init(InitCommandOptions {
            config,
            token_file,
            agent,
            agent_command,
            workspace,
            workspace_path,
            pass_env,
            buzz,
            mcp_policy,
            force,
        }),
        Command::Doctor {
            config,
            token_file,
            url,
        } => run_doctor(config, token_file, url.as_ref()).await,
        Command::Service {
            command: ServiceCommand::Generate { user },
        } => {
            if !user {
                return Err(Error::Config(
                    "service generation currently requires --user".into(),
                ));
            }
            let executable = std::env::current_exe().map_err(|error| {
                Error::Config(format!("cannot resolve the current executable: {error}"))
            })?;
            let service = generate_user_service(&executable)?;
            print!("{service}");
            eprintln!("acp-tunnel: save this unit as ~/.config/systemd/user/acp-tunnel.service");
            Ok(())
        }
        Command::TestAgent {
            uncooperative,
            spawn_grandchild,
        } => run_test_agent(uncooperative, spawn_grandchild).await,
    }
}

struct InitCommandOptions {
    config: Option<PathBuf>,
    token_file: Option<PathBuf>,
    agent: Option<String>,
    agent_command: Option<PathBuf>,
    workspace: Option<String>,
    workspace_path: Option<PathBuf>,
    pass_env: Vec<String>,
    buzz: bool,
    mcp_policy: Option<InitMcpPolicy>,
    force: bool,
}

fn run_init(options: InitCommandOptions) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let config_path = resolve_server_config_file(options.config)?;
    let token_path = options
        .token_file
        .or_else(default_token_file)
        .ok_or_else(|| Error::Config("use --token-file because HOME is unavailable".into()))?;
    let current_directory = std::env::current_dir()
        .map_err(|error| Error::Config(format!("cannot read the current directory: {error}")))?;

    let agent_id = required_value(options.agent, interactive, "Agent ID", Some("agent"))?;
    let command = required_path(options.agent_command, interactive, "Agent command", None)?;
    let workspace_id = required_value(
        options.workspace,
        interactive,
        "Workspace ID",
        default_workspace_id(&current_directory).as_deref(),
    )?;
    let workspace_path = required_path(
        options.workspace_path,
        interactive,
        "Workspace path",
        Some(&current_directory),
    )?;
    let pass_env = if options.pass_env.is_empty() {
        if interactive {
            parse_environment_names(&prompt("Inherited environment names", Some("HOME,PATH"))?)?
        } else {
            BTreeSet::from(["HOME".into(), "PATH".into()])
        }
    } else {
        options.pass_env.into_iter().collect()
    };
    let buzz = if options.buzz || !interactive {
        options.buzz
    } else {
        prompt_yes_no("Accept Buzz session variables", true)?
    };
    let client_env_allowlist = if buzz {
        BUZZ_CLIENT_ENVIRONMENT.map(str::to_owned).into()
    } else {
        BTreeSet::new()
    };
    let mcp_policy = match options.mcp_policy {
        Some(policy) => policy,
        None if interactive => prompt_mcp_policy()?,
        None => InitMcpPolicy::Passthrough,
    };
    if matches!(mcp_policy, InitMcpPolicy::Passthrough) {
        eprintln!(
            "acp-tunnel: WARNING: MCP passthrough permits authenticated clients to run remote commands"
        );
    }

    let report = initialize_server(InitOptions {
        config_path,
        token_path,
        agent_id: agent_id.clone(),
        command,
        workspace_id: workspace_id.clone(),
        workspace_path,
        pass_env,
        client_env_allowlist,
        mcp_policy: mcp_policy.into(),
        force: options.force,
    })?;
    println!("Created configuration: {}", report.config_path.display());
    if report.token_created {
        println!("Created bearer token: {}", report.token_path.display());
    } else {
        println!(
            "Using existing bearer token: {}",
            report.token_path.display()
        );
    }
    println!("Agent executable: {}", report.command.display());
    println!("Workspace: {}", report.workspace_path.display());
    println!("Run `acp-tunnel doctor`, then run `acp-tunnel serve`.");
    println!("Configure the connector with --agent {agent_id} --workspace {workspace_id}.");
    Ok(())
}

async fn run_doctor(
    config: Option<PathBuf>,
    token_file: Option<PathBuf>,
    url: Option<&Url>,
) -> Result<()> {
    let config = resolve_server_config_file(config)?;
    let mut report = diagnose_server(&config, token_file.as_deref(), url);
    if let Some(url) = url {
        report.append(acp_tunnel::setup::diagnose_websocket_endpoint(url).await);
    }
    for notice in &report.notices {
        let level = match notice.level {
            DoctorLevel::Ok => "ok",
            DoctorLevel::Warning => "warning",
            DoctorLevel::Error => "error",
        };
        println!("[{level}] {}", notice.message);
    }
    if report.has_errors() {
        Err(Error::Config("doctor found one or more errors".into()))
    } else {
        Ok(())
    }
}

fn required_value(
    value: Option<String>,
    interactive: bool,
    label: &str,
    default: Option<&str>,
) -> Result<String> {
    match value {
        Some(value) => Ok(value),
        None if interactive => prompt(label, default),
        None => Err(Error::Config(format!(
            "use --{} when stdin is not a terminal",
            label.to_ascii_lowercase().replace(' ', "-")
        ))),
    }
}

fn required_path(
    value: Option<PathBuf>,
    interactive: bool,
    label: &str,
    default: Option<&Path>,
) -> Result<PathBuf> {
    match value {
        Some(value) => Ok(value),
        None if interactive => {
            let default = default.map(|path| path.to_string_lossy().into_owned());
            prompt(label, default.as_deref()).map(PathBuf::from)
        }
        None => Err(Error::Config(format!(
            "use --{} when stdin is not a terminal",
            label.to_ascii_lowercase().replace(' ', "-")
        ))),
    }
}

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}: "),
    }
    std::io::stdout()
        .flush()
        .map_err(|error| Error::Config(format!("cannot write prompt: {error}")))?;
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|error| Error::Config(format!("cannot read prompt response: {error}")))?;
    let input = input.trim();
    if input.is_empty() {
        default.map(str::to_owned).ok_or_else(|| {
            Error::Config(format!("{} must not be empty", label.to_ascii_lowercase()))
        })
    } else {
        Ok(input.to_owned())
    }
}

fn prompt_yes_no(label: &str, default: bool) -> Result<bool> {
    let marker = if default { "Y/n" } else { "y/N" };
    let answer = prompt(label, Some(marker))?;
    if answer == marker {
        return Ok(default);
    }
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(Error::Config(format!(
            "{label} requires a yes or no response"
        ))),
    }
}

fn prompt_mcp_policy() -> Result<InitMcpPolicy> {
    let policy = prompt(
        "MCP policy (passthrough, allowlisted, or deny)",
        Some("passthrough"),
    )?;
    match policy.to_ascii_lowercase().as_str() {
        "passthrough" => Ok(InitMcpPolicy::Passthrough),
        "allowlisted" => Ok(InitMcpPolicy::Allowlisted),
        "deny" => Ok(InitMcpPolicy::Deny),
        _ => Err(Error::Config("unknown MCP policy".into())),
    }
}

fn parse_environment_names(text: &str) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for name in text
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        acp_tunnel::config::validate_environment_name("generated agent", "environment", name)?;
        names.insert(name.to_owned());
    }
    Ok(names)
}

fn default_workspace_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let mut id = String::new();
    let mut separator = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            id.push(character);
            separator = false;
        } else if !id.is_empty() && !separator {
            id.push('-');
            separator = true;
        }
    }
    while id.ends_with('-') {
        id.pop();
    }
    (!id.is_empty()).then_some(id)
}

#[cfg(unix)]
fn install_connector_signal_handler(handle: ShutdownHandle) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|error| Error::Config(format!("cannot install SIGTERM handler: {error}")))?;
    let sigterm_handle = handle.clone();
    tokio::spawn(async move {
        if sigterm.recv().await.is_some() {
            let _ = sigterm_handle.shutdown(ShutdownReason::Sigterm);
        }
    });
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = handle.shutdown(ShutdownReason::Interrupt);
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn install_connector_signal_handler(handle: ShutdownHandle) -> Result<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = handle.shutdown(ShutdownReason::Interrupt);
        }
    });
    Ok(())
}

#[cfg(unix)]
fn install_server_signal_handler(shutdown: CancellationToken) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|error| Error::Config(format!("cannot install SIGTERM handler: {error}")))?;
    let sigterm_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if sigterm.recv().await.is_some() {
            sigterm_shutdown.cancel();
        }
    });
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown.cancel();
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn install_server_signal_handler(shutdown: CancellationToken) -> Result<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shutdown.cancel();
        }
    });
    Ok(())
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

async fn run_test_agent(uncooperative: bool, spawn_grandchild: bool) -> Result<()> {
    if spawn_grandchild {
        let executable = std::env::current_exe()
            .map_err(|error| Error::Process(format!("cannot find test executable: {error}")))?;
        let grandchild = std::process::Command::new(executable)
            .args(["__test-agent", "--uncooperative"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| {
                Error::Process(format!("cannot start test-agent grandchild: {error}"))
            })?;
        eprintln!("fake-agent grandchild-pid={}", grandchild.id());
    }

    #[cfg(unix)]
    let mut sigterm = if uncooperative {
        Some(
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(
                |error| Error::Process(format!("cannot install test SIGTERM handler: {error}")),
            )?,
        )
    } else {
        None
    };

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
            Some("test/environment") => {
                let name = request
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                write_json(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "result":{"value":std::env::var(name).ok()}
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

    if uncooperative {
        #[cfg(unix)]
        if let Some(signal) = sigterm.as_mut() {
            while signal.recv().await.is_some() {}
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
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
