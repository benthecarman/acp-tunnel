use std::{
    collections::{HashMap, VecDeque},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{
        Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::{MissedTickBehavior, timeout},
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    Error, Result,
    auth::{Authenticator, StaticTokenAuthenticator, parse_bearer},
    client::keepalive_nonce,
    config::ServerConfig,
    credentials::SecretToken,
    policy::{AcpPolicy, PolicyOutcome},
    process::{AgentProcess, exit_details},
    protocol::{AckStream, Envelope, OpenRequest, ShutdownReason, TUNNEL_VERSION},
};

/// Shared server state used by HTTP handlers and tunnel tasks.
#[derive(Clone)]
pub struct ServerState {
    config: Arc<ServerConfig>,
    authenticator: Arc<dyn Authenticator>,
    shutdown: CancellationToken,
    ready: Arc<AtomicBool>,
    sessions: Arc<Mutex<HashMap<String, ResumeEntry>>>,
}

#[derive(Clone)]
struct ResumeEntry {
    resume_token: String,
    agent: String,
    workspace: String,
    attachments: mpsc::Sender<WebSocket>,
}

impl ServerState {
    /// Creates server state from validated configuration and an authenticator.
    pub fn new(
        config: Arc<ServerConfig>,
        authenticator: Arc<dyn Authenticator>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            config,
            authenticator,
            shutdown,
            ready: Arc::new(AtomicBool::new(true)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Builds the HTTP router, including health, readiness, and tunnel routes.
pub fn router(state: ServerState) -> Router {
    let tunnel = Router::new()
        .route("/v1/tunnel", get(upgrade_tunnel))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            enforce_upgrade_security,
        ));
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .merge(tunnel)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok\n"
}

async fn ready(State(state): State<ServerState>) -> Response {
    if state.ready.load(Ordering::Relaxed) {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response()
    }
}

async fn upgrade_tunnel(State(state): State<ServerState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(state.config.max_frame_bytes)
        .max_frame_size(state.config.max_frame_bytes)
        .on_upgrade(move |socket| run_tunnel(socket, state))
}

async fn enforce_upgrade_security(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    if !authorized(&state, request.headers()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    if !origin_allowed(&state.config, request.headers()) {
        return (StatusCode::FORBIDDEN, "forbidden\n").into_response();
    }
    next.run(request).await
}

fn authorized(state: &ServerState, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_bearer)
        .is_some_and(|token| state.authenticator.authenticate(token))
}

fn origin_allowed(config: &ServerConfig, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .ok()
            .is_some_and(|origin| config.allowed_origins.contains(origin)),
    }
}

#[derive(Default)]
struct Counters {
    inbound_frames: AtomicU64,
    inbound_bytes: AtomicU64,
    outbound_frames: AtomicU64,
    outbound_bytes: AtomicU64,
    dropped_stderr_frames: AtomicU64,
}

struct ReplayFrame {
    sequence: u64,
    payload: String,
    last_sent_generation: u64,
}

enum AgentEvent {
    Acp(String),
    StdoutError,
    StdoutClosed,
}

enum TransportCommand {
    Envelope(Envelope),
    Exit {
        envelope: Envelope,
        sent: oneshot::Sender<()>,
    },
    ShutdownComplete {
        envelope: Envelope,
        sent: oneshot::Sender<()>,
    },
}

struct TransportEvent {
    generation: u64,
    kind: TransportEventKind,
}

enum TransportEventKind {
    Envelope(Envelope),
    Activity,
    Writable,
    Disconnected,
    Closed,
    InvalidEnvelope,
}

struct TransportHandle {
    generation: u64,
    commands: mpsc::Sender<TransportCommand>,
    task: JoinHandle<()>,
}

async fn run_tunnel(mut socket: WebSocket, state: ServerState) {
    let mut open = match read_open(&mut socket, &state).await {
        Ok(open) => open,
        Err(error) => {
            warn!(
                error_category = error_category(&error),
                error = %error,
                "tunnel opening failed"
            );
            return;
        }
    };

    if open.resume.is_some() {
        if let Err(error) = resume_tunnel(socket, &state, open).await {
            warn!(
                error_category = error_category(&error),
                error = %error,
                "tunnel resume failed"
            );
        }
        return;
    }

    let connection_id = Uuid::new_v4().to_string();
    let (process, policy) = match prepare_agent(&mut socket, &state, &open).await {
        Ok(prepared) => prepared,
        Err(error) => {
            warn!(
                connection_id,
                error_category = error_category(&error),
                error = %error,
                "tunnel failed before agent start"
            );
            return;
        }
    };
    open.client_environment.clear();

    let resume_token = new_resume_token();
    let (attachment_tx, attachment_rx) = mpsc::channel(2);
    state.sessions.lock().await.insert(
        connection_id.clone(),
        ResumeEntry {
            resume_token: resume_token.clone(),
            agent: open.agent.clone(),
            workspace: open.workspace.clone(),
            attachments: attachment_tx,
        },
    );
    let result = run_agent_session(
        socket,
        &state,
        &connection_id,
        &resume_token,
        attachment_rx,
        process,
        policy,
        open,
    )
    .await;
    state.sessions.lock().await.remove(&connection_id);

    if let Err(error) = result {
        warn!(
            connection_id,
            error_category = error_category(&error),
            error = %error,
            "tunnel connection ended with error"
        );
    }
}

async fn read_open(socket: &mut WebSocket, state: &ServerState) -> Result<OpenRequest> {
    let first = match timeout(state.config.connection_timeout(), socket.recv()).await {
        Ok(Some(Ok(message))) => message,
        Ok(Some(Err(source))) => {
            let error = Error::Network(format!("cannot read open envelope: {source}"));
            fail_socket(socket, "invalid_open", &error).await;
            return Err(error);
        }
        Ok(None) => {
            return Err(Error::Protocol(
                "connection closed before open envelope".into(),
            ));
        }
        Err(_) => {
            let error = Error::Timeout("waiting for open envelope");
            fail_socket(socket, "open_timeout", &error).await;
            return Err(error);
        }
    };
    let text = match first {
        Message::Text(text) => text,
        _ => {
            let error = Error::Protocol("opening message must be WebSocket text".into());
            fail_socket(socket, "invalid_open", &error).await;
            return Err(error);
        }
    };
    match Envelope::from_text(&text).and_then(Envelope::into_open) {
        Ok(open) => Ok(open),
        Err(error) => {
            let code = if error.to_string().contains("unsupported tunnel version") {
                "unsupported_tunnel_version"
            } else {
                "invalid_open"
            };
            fail_socket(socket, code, &error).await;
            Err(error)
        }
    }
}

async fn prepare_agent(
    socket: &mut WebSocket,
    state: &ServerState,
    open: &OpenRequest,
) -> Result<(AgentProcess, AcpPolicy)> {
    let agent = match state.config.agents.get(&open.agent) {
        Some(agent) => agent,
        None => {
            let error = Error::Protocol(format!("unknown agent {:?}", open.agent));
            fail_socket(socket, "unknown_agent", &error).await;
            return Err(error);
        }
    };
    if !agent.workspaces.contains(&open.workspace) {
        let error = Error::Protocol(format!(
            "workspace {:?} is not allowed for agent {:?}",
            open.workspace, open.agent
        ));
        fail_socket(socket, "unknown_workspace", &error).await;
        return Err(error);
    }
    let workspace = match state.config.workspaces.get(&open.workspace) {
        Some(workspace) => workspace,
        None => {
            let error = Error::Protocol(format!("unknown workspace {:?}", open.workspace));
            fail_socket(socket, "unknown_workspace", &error).await;
            return Err(error);
        }
    };
    let process = match AgentProcess::spawn(agent, &workspace.path, &open.client_environment) {
        Ok(process) => process,
        Err(error) => {
            let code = if matches!(&error, Error::Protocol(_)) {
                "client_environment_rejected"
            } else {
                "agent_start_failed"
            };
            fail_socket(socket, code, &error).await;
            return Err(error);
        }
    };
    let policy = AcpPolicy::new(
        workspace.path.to_string_lossy().into_owned(),
        state.config.rewrite_cwd,
        agent.mcp_policy,
        state.config.mcp_servers.clone(),
    );
    Ok((process, policy))
}

async fn resume_tunnel(
    mut socket: WebSocket,
    state: &ServerState,
    open: OpenRequest,
) -> Result<()> {
    if !open.client_environment.is_empty() {
        let error = Error::Protocol("resume request was rejected".into());
        fail_socket(&mut socket, "resume_rejected", &error).await;
        return Err(error);
    }
    let resume = open
        .resume
        .as_ref()
        .ok_or_else(|| Error::Protocol("resume credentials are missing".into()))?;
    let entry = state
        .sessions
        .lock()
        .await
        .get(&resume.connection_id)
        .cloned();
    let Some(entry) = entry else {
        let error = Error::Protocol("resume request was rejected".into());
        fail_socket(&mut socket, "resume_rejected", &error).await;
        return Err(error);
    };
    let token_authenticator =
        StaticTokenAuthenticator::new(SecretToken::new(entry.resume_token.clone())?);
    if entry.agent != open.agent
        || entry.workspace != open.workspace
        || !token_authenticator.authenticate(&resume.resume_token)
    {
        let error = Error::Protocol("resume request was rejected".into());
        fail_socket(&mut socket, "resume_rejected", &error).await;
        return Err(error);
    }
    timeout(
        state.config.connection_timeout(),
        entry.attachments.send(socket),
    )
    .await
    .map_err(|_| Error::Timeout("attaching resumed WebSocket"))?
    .map_err(|_| Error::Protocol("resume session is no longer available".into()))
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_session(
    socket: WebSocket,
    state: &ServerState,
    connection_id: &str,
    resume_token: &str,
    mut attachments: mpsc::Receiver<WebSocket>,
    mut process: AgentProcess,
    policy: AcpPolicy,
    open: OpenRequest,
) -> Result<()> {
    let pid = process.pid;
    let mut stdin = Some(BufWriter::new(process.take_stdin()?));
    let stdout = process.take_stdout()?;
    let stderr = process.take_stderr()?;
    let counters = Arc::new(Counters::default());
    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(1);
    let (diagnostic_tx, mut diagnostic_rx) =
        mpsc::channel::<String>(state.config.diagnostic_channel_capacity);
    let mut stdout_task = spawn_agent_stdout(
        stdout,
        agent_tx,
        counters.clone(),
        state.config.max_frame_bytes,
    );
    let mut stderr_task = spawn_stderr(
        stderr,
        diagnostic_tx,
        counters.clone(),
        state.config.diagnostic_line_bytes,
    );
    let event_capacity = state.config.channel_capacity.saturating_mul(2).max(8);
    let (transport_event_tx, mut transport_event_rx) =
        mpsc::channel::<TransportEvent>(event_capacity);
    let mut pending = VecDeque::<ReplayFrame>::new();
    let mut pending_bytes = 0_usize;
    let mut next_server_sequence = 1_u64;
    let mut expected_client_sequence = 1_u64;
    let mut generation = 1_u64;
    let mut transport = Some(
        attach_transport(
            socket,
            generation,
            transport_event_tx.clone(),
            state.config.channel_capacity,
            Envelope::Ready {
                tunnel_version: TUNNEL_VERSION,
                connection_id: connection_id.to_owned(),
                resume_token: Some(resume_token.to_owned()),
                resumed: false,
            },
            None,
            &mut pending,
            state.config.connection_timeout(),
        )
        .await?,
    );
    let mut detached_deadline = None::<tokio::time::Instant>;
    let mut last_received = Instant::now();
    let mut child_status = None::<ExitStatus>;
    let mut resumed_count = 0_u64;
    let mut exit_was_sent = false;
    let mut shutdown_reason = None::<ShutdownReason>;
    let mut keepalive = tokio::time::interval(state.config.keepalive_interval());
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!(
        connection_id,
        agent_id = open.agent,
        workspace_id = open.workspace,
        child_pid = pid,
        client_name = open.client_info.name,
        client_version = open.client_info.version,
        "tunnel opened"
    );

    let mut end_category = None::<String>;
    while end_category.is_none() {
        if let Some(current) = transport.as_ref()
            && !pump_replay(current, &mut pending)
        {
            detach_transport(
                &mut transport,
                &mut detached_deadline,
                state.config.reconnect_grace(),
            );
        }

        if let (Some(status), Some(current)) = (child_status.as_ref(), transport.as_ref()) {
            let all_enqueued = pending
                .iter()
                .all(|frame| frame.last_sent_generation == current.generation);
            if all_enqueued {
                if send_exit(current, status, state.config.shutdown_timeout()).await {
                    exit_was_sent = true;
                    end_category = Some("child_exit".into());
                    continue;
                }
                detach_transport(
                    &mut transport,
                    &mut detached_deadline,
                    state.config.reconnect_grace(),
                );
            }
        }

        let can_read_agent = child_status.is_none()
            && pending.len() < state.config.max_replay_frames
            && pending_bytes
                <= state
                    .config
                    .max_replay_bytes
                    .saturating_sub(state.config.max_frame_bytes);

        tokio::select! {
            status = process.wait(), if child_status.is_none() => {
                child_status = Some(status?);
            }
            attachment = attachments.recv() => {
                if let Some(socket) = attachment {
                    if let Some(previous) = transport.take() {
                        previous.task.abort();
                    }
                    generation = generation.checked_add(1)
                        .ok_or_else(|| Error::Protocol("transport generation exhausted".into()))?;
                    let client_ack = expected_client_sequence.checked_sub(1).map(|sequence| {
                        Envelope::Ack {
                            stream: AckStream::ClientToServer,
                            sequence,
                        }
                    });
                    match attach_transport(
                        socket,
                        generation,
                        transport_event_tx.clone(),
                        state.config.channel_capacity,
                        Envelope::Ready {
                            tunnel_version: TUNNEL_VERSION,
                            connection_id: connection_id.to_owned(),
                            resume_token: Some(resume_token.to_owned()),
                            resumed: true,
                        },
                        client_ack,
                        &mut pending,
                        state.config.connection_timeout(),
                    ).await {
                        Ok(attached) => {
                            transport = Some(attached);
                            detached_deadline = None;
                            last_received = Instant::now();
                            resumed_count = resumed_count.saturating_add(1);
                            info!(
                                connection_id,
                                agent_id = open.agent,
                                workspace_id = open.workspace,
                                child_pid = pid,
                                resumed_count,
                                "tunnel transport resumed"
                            );
                        }
                        Err(error) => {
                            warn!(
                                connection_id,
                                error = %error,
                                "failed to attach resumed transport"
                            );
                        }
                    }
                }
            }
            _ = state.shutdown.cancelled() => {
                end_category = Some("server_shutdown".into());
            }
            _ = keepalive.tick() => {
                if transport.is_some()
                    && last_received.elapsed() > state.config.keepalive_timeout()
                {
                    detach_transport(
                        &mut transport,
                        &mut detached_deadline,
                        state.config.reconnect_grace(),
                    );
                } else if let Some(current) = transport.as_ref() {
                    let _ = current.commands.try_send(TransportCommand::Envelope(
                        Envelope::Ping {
                            nonce: keepalive_nonce(),
                        }
                    ));
                }
            }
            event = transport_event_rx.recv() => {
                let Some(event) = event else {
                    end_category = Some("transport_task_stopped".into());
                    continue;
                };
                if transport.as_ref().map(|current| current.generation) != Some(event.generation) {
                    continue;
                }
                match event.kind {
                    TransportEventKind::Envelope(envelope) => {
                        last_received = Instant::now();
                        handle_client_envelope(
                            envelope,
                            &policy,
                            stdin.as_mut().ok_or_else(|| {
                                Error::Process("agent stdin closed before session ended".into())
                            })?,
                            transport.as_ref(),
                            &mut pending,
                            &mut pending_bytes,
                            &mut next_server_sequence,
                            &mut expected_client_sequence,
                            &counters,
                            &state.config,
                            &mut end_category,
                            &mut shutdown_reason,
                        ).await?;
                        if shutdown_reason.is_some() {
                            state.sessions.lock().await.remove(connection_id);
                            end_category = Some("client_shutdown".into());
                        }
                    }
                    TransportEventKind::Activity => {
                        last_received = Instant::now();
                    }
                    TransportEventKind::Writable => {}
                    TransportEventKind::Disconnected => {
                        detach_transport(
                            &mut transport,
                            &mut detached_deadline,
                            state.config.reconnect_grace(),
                        );
                        info!(
                            connection_id,
                            agent_id = open.agent,
                            workspace_id = open.workspace,
                            child_pid = pid,
                            "tunnel transport detached; awaiting reconnect"
                        );
                    }
                    TransportEventKind::Closed => {
                        detach_transport(
                            &mut transport,
                            &mut detached_deadline,
                            state.config.reconnect_grace(),
                        );
                    }
                    TransportEventKind::InvalidEnvelope => {
                        end_category = Some("invalid_envelope".into());
                    }
                }
            }
            agent = agent_rx.recv(), if can_read_agent => {
                match agent {
                    Some(AgentEvent::Acp(payload)) => {
                        queue_server_frame(
                            payload,
                            &mut pending,
                            &mut pending_bytes,
                            &mut next_server_sequence,
                        )?;
                    }
                    Some(AgentEvent::StdoutError) => {
                        end_category = Some("agent_stdout_line".into());
                    }
                    Some(AgentEvent::StdoutClosed) | None => {}
                }
            }
            diagnostic = diagnostic_rx.recv() => {
                if let Some(payload) = diagnostic
                    && let Some(current) = transport.as_ref()
                    && current.commands.try_send(TransportCommand::Envelope(
                        Envelope::Stderr { payload }
                    )).is_err()
                {
                    counters.dropped_stderr_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ = wait_for_deadline(detached_deadline), if detached_deadline.is_some() => {
                end_category = Some("reconnect_timeout".into());
            }
        }
    }

    if end_category.as_deref() == Some("client_shutdown") {
        if let Some(mut agent_stdin) = stdin.take() {
            let _ = agent_stdin.flush().await;
            let _ = agent_stdin.shutdown().await;
            drop(agent_stdin);
        }
        if child_status.is_none() {
            child_status = Some(
                process
                    .graceful_shutdown_and_reap(
                        state.config.shutdown_timeout(),
                        state.config.shutdown_timeout(),
                    )
                    .await?,
            );
        }
    } else if child_status.is_none() {
        child_status = Some(
            process
                .terminate_and_reap(state.config.shutdown_timeout())
                .await?,
        );
    }
    let status =
        child_status.ok_or_else(|| Error::Process("agent ended without an exit status".into()))?;
    let (code, signal) = exit_details(&status);
    let end_category = end_category.unwrap_or_else(|| "unknown".into());

    if end_category == "client_shutdown" {
        if let Some(current) = transport.as_ref() {
            let _ = send_shutdown_complete(current, &status, state.config.shutdown_timeout()).await;
        }
    } else if let Some(current) = transport.as_ref()
        && !exit_was_sent
        && !matches!(end_category.as_str(), "client_close" | "reconnect_timeout")
    {
        let _ = current
            .commands
            .send(TransportCommand::Envelope(Envelope::Error {
                code: end_category.clone(),
                message: format!("tunnel ended: {end_category}"),
            }))
            .await;
        let _ = send_exit(current, &status, state.config.shutdown_timeout()).await;
    }

    stdout_task.abort();
    stderr_task.abort();
    let _ = (&mut stdout_task).await;
    let _ = (&mut stderr_task).await;
    if let Some(current) = transport.take() {
        current.task.abort();
    }

    info!(
        connection_id,
        agent_id = open.agent,
        workspace_id = open.workspace,
        child_pid = pid,
        exit_code = code,
        signal,
        end_category,
        resumed_count,
        inbound_frames = counters.inbound_frames.load(Ordering::Relaxed),
        inbound_bytes = counters.inbound_bytes.load(Ordering::Relaxed),
        outbound_frames = counters.outbound_frames.load(Ordering::Relaxed),
        outbound_bytes = counters.outbound_bytes.load(Ordering::Relaxed),
        dropped_stderr_frames = counters.dropped_stderr_frames.load(Ordering::Relaxed),
        "tunnel closed"
    );

    if (end_category == "child_exit" && status.success()) || end_category == "client_shutdown" {
        Ok(())
    } else {
        Err(match end_category.as_str() {
            "server_shutdown" => Error::Network("server is shutting down".into()),
            "reconnect_timeout" => Error::Timeout("waiting for client reconnect"),
            category => Error::Network(format!("tunnel ended: {category}")),
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_envelope(
    envelope: Envelope,
    policy: &AcpPolicy,
    stdin: &mut BufWriter<tokio::process::ChildStdin>,
    transport: Option<&TransportHandle>,
    pending: &mut VecDeque<ReplayFrame>,
    pending_bytes: &mut usize,
    next_server_sequence: &mut u64,
    expected_client_sequence: &mut u64,
    counters: &Counters,
    config: &ServerConfig,
    end_category: &mut Option<String>,
    shutdown_reason: &mut Option<ShutdownReason>,
) -> Result<()> {
    match envelope {
        Envelope::Acp {
            sequence: Some(sequence),
            payload,
        } => {
            counters.inbound_frames.fetch_add(1, Ordering::Relaxed);
            counters.inbound_bytes.fetch_add(
                u64::try_from(payload.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            if payload.len() > config.max_frame_bytes {
                *end_category = Some("oversized_acp_line".into());
                return Ok(());
            }
            if sequence > *expected_client_sequence {
                *end_category = Some("client_sequence_gap".into());
                return Ok(());
            }
            if sequence < *expected_client_sequence {
                send_client_ack(transport, *expected_client_sequence).await;
                return Ok(());
            }
            match policy.apply(&payload)? {
                PolicyOutcome::Forward(line) => {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                PolicyOutcome::Reject(line) => {
                    if !replay_fits(pending, *pending_bytes, &line, config) {
                        *end_category = Some("replay_buffer_exhausted".into());
                        return Ok(());
                    }
                    queue_server_frame(line, pending, pending_bytes, next_server_sequence)?;
                }
            }
            *expected_client_sequence = expected_client_sequence
                .checked_add(1)
                .ok_or_else(|| Error::Protocol("client ACP sequence exhausted".into()))?;
            send_client_ack(transport, *expected_client_sequence).await;
        }
        Envelope::Acp { sequence: None, .. } => {
            *end_category = Some("missing_sequence".into());
        }
        Envelope::Ack {
            stream: AckStream::ServerToClient,
            sequence,
        } => {
            if sequence >= *next_server_sequence {
                *end_category = Some("invalid_acknowledgement".into());
                return Ok(());
            }
            while pending
                .front()
                .is_some_and(|frame| frame.sequence <= sequence)
            {
                if let Some(frame) = pending.pop_front() {
                    *pending_bytes = pending_bytes.saturating_sub(frame.payload.len());
                }
            }
        }
        Envelope::Ack { .. } => {
            *end_category = Some("unexpected_acknowledgement".into());
        }
        Envelope::Ping { nonce } => {
            if let Some(transport) = transport {
                let _ = transport
                    .commands
                    .send(TransportCommand::Envelope(Envelope::Pong { nonce }))
                    .await;
            }
        }
        Envelope::Pong { .. } => {}
        Envelope::Shutdown { reason } => {
            if shutdown_reason.is_none() {
                *shutdown_reason = Some(reason);
            }
        }
        _ => {
            *end_category = Some("unexpected_envelope".into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn attach_transport(
    socket: WebSocket,
    generation: u64,
    events: mpsc::Sender<TransportEvent>,
    capacity: usize,
    ready: Envelope,
    client_ack: Option<Envelope>,
    pending: &mut VecDeque<ReplayFrame>,
    setup_timeout: Duration,
) -> Result<TransportHandle> {
    let (commands, command_rx) = mpsc::channel(capacity);
    let task = spawn_transport(socket, generation, command_rx, events);
    let setup = async {
        commands
            .send(TransportCommand::Envelope(ready))
            .await
            .map_err(|_| Error::Network("transport stopped before ready".into()))?;
        if let Some(ack) = client_ack {
            commands
                .send(TransportCommand::Envelope(ack))
                .await
                .map_err(|_| {
                    Error::Network("transport stopped before resume acknowledgement".into())
                })?;
        }
        for frame in pending {
            commands
                .send(TransportCommand::Envelope(Envelope::Acp {
                    sequence: Some(frame.sequence),
                    payload: frame.payload.clone(),
                }))
                .await
                .map_err(|_| Error::Network("transport stopped during replay".into()))?;
            frame.last_sent_generation = generation;
        }
        Ok::<(), Error>(())
    };
    if let Err(error) = timeout(setup_timeout, setup)
        .await
        .map_err(|_| Error::Timeout("preparing resumed transport"))?
    {
        task.abort();
        return Err(error);
    }
    Ok(TransportHandle {
        generation,
        commands,
        task,
    })
}

fn spawn_transport(
    mut socket: WebSocket,
    generation: u64,
    mut commands: mpsc::Receiver<TransportCommand>,
    events: mpsc::Sender<TransportEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    };
                    let (message, sent) = match command {
                        TransportCommand::Envelope(envelope) => {
                            let text = match envelope.to_text() {
                                Ok(text) => text,
                                Err(_) => {
                                    let _ = events.try_send(TransportEvent {
                                        generation,
                                        kind: TransportEventKind::InvalidEnvelope,
                                    });
                                    return;
                                }
                            };
                            (Message::Text(text.into()), None)
                        }
                        TransportCommand::Exit { envelope, sent } => {
                            let text = match envelope.to_text() {
                                Ok(text) => text,
                                Err(_) => return,
                            };
                            (Message::Text(text.into()), Some(sent))
                        }
                        TransportCommand::ShutdownComplete { envelope, sent } => {
                            let text = match envelope.to_text() {
                                Ok(text) => text,
                                Err(_) => return,
                            };
                            if socket.send(Message::Text(text.into())).await.is_err() {
                                return;
                            }
                            let _ = socket.send(Message::Close(None)).await;
                            let _ = sent.send(());
                            return;
                        }
                    };
                    if socket.send(message).await.is_err() {
                        let _ = events.try_send(TransportEvent {
                            generation,
                            kind: TransportEventKind::Disconnected,
                        });
                        return;
                    }
                    if let Some(sent) = sent {
                        let _ = sent.send(());
                    }
                    let _ = events.try_send(TransportEvent {
                        generation,
                        kind: TransportEventKind::Writable,
                    });
                }
                message = socket.recv() => {
                    let kind = match message {
                        Some(Ok(Message::Text(text))) => match Envelope::from_text(&text) {
                            Ok(envelope) => TransportEventKind::Envelope(envelope),
                            Err(_) => TransportEventKind::InvalidEnvelope,
                        },
                        Some(Ok(Message::Ping(payload))) => {
                            if socket.send(Message::Pong(payload)).await.is_err() {
                                TransportEventKind::Disconnected
                            } else {
                                TransportEventKind::Activity
                            }
                        }
                        Some(Ok(Message::Pong(_))) => TransportEventKind::Activity,
                        Some(Ok(Message::Close(_))) => TransportEventKind::Closed,
                        Some(Ok(Message::Binary(_))) => TransportEventKind::InvalidEnvelope,
                        Some(Err(_)) | None => TransportEventKind::Disconnected,
                    };
                    let terminal = matches!(
                        kind,
                        TransportEventKind::Disconnected
                            | TransportEventKind::Closed
                            | TransportEventKind::InvalidEnvelope
                    );
                    if events.send(TransportEvent { generation, kind }).await.is_err() || terminal {
                        return;
                    }
                }
            }
        }
    })
}

fn spawn_agent_stdout(
    stdout: tokio::process::ChildStdout,
    events: mpsc::Sender<AgentEvent>,
    counters: Arc<Counters>,
    max_frame_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(max_frame_bytes));
        while let Some(line) = lines.next().await {
            match line {
                Ok(payload) => {
                    counters.outbound_frames.fetch_add(1, Ordering::Relaxed);
                    counters.outbound_bytes.fetch_add(
                        u64::try_from(payload.len()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                    if events.send(AgentEvent::Acp(payload)).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = events.send(AgentEvent::StdoutError).await;
                    return;
                }
            }
        }
        let _ = events.send(AgentEvent::StdoutClosed).await;
    })
}

fn spawn_stderr(
    stderr: tokio::process::ChildStderr,
    diagnostics: mpsc::Sender<String>,
    counters: Arc<Counters>,
    max_line_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = FramedRead::new(stderr, LinesCodec::new_with_max_length(max_line_bytes));
        while let Some(line) = lines.next().await {
            match line {
                Ok(payload) => {
                    if diagnostics.try_send(payload).is_err() {
                        counters
                            .dropped_stderr_frames
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    counters
                        .dropped_stderr_frames
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })
}

fn replay_fits(
    pending: &VecDeque<ReplayFrame>,
    pending_bytes: usize,
    payload: &str,
    config: &ServerConfig,
) -> bool {
    pending.len() < config.max_replay_frames
        && pending_bytes.saturating_add(payload.len()) <= config.max_replay_bytes
}

fn queue_server_frame(
    payload: String,
    pending: &mut VecDeque<ReplayFrame>,
    pending_bytes: &mut usize,
    next_sequence: &mut u64,
) -> Result<()> {
    let sequence = *next_sequence;
    *next_sequence = next_sequence
        .checked_add(1)
        .ok_or_else(|| Error::Protocol("server ACP sequence exhausted".into()))?;
    *pending_bytes = pending_bytes.saturating_add(payload.len());
    pending.push_back(ReplayFrame {
        sequence,
        payload,
        last_sent_generation: 0,
    });
    Ok(())
}

fn pump_replay(transport: &TransportHandle, pending: &mut VecDeque<ReplayFrame>) -> bool {
    for frame in pending {
        if frame.last_sent_generation == transport.generation {
            continue;
        }
        match transport
            .commands
            .try_send(TransportCommand::Envelope(Envelope::Acp {
                sequence: Some(frame.sequence),
                payload: frame.payload.clone(),
            })) {
            Ok(()) => frame.last_sent_generation = transport.generation,
            Err(mpsc::error::TrySendError::Full(_)) => break,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
        }
    }
    true
}

async fn send_client_ack(transport: Option<&TransportHandle>, expected_sequence: u64) {
    let Some(sequence) = expected_sequence.checked_sub(1) else {
        return;
    };
    if let Some(transport) = transport {
        let _ = transport
            .commands
            .send(TransportCommand::Envelope(Envelope::Ack {
                stream: AckStream::ClientToServer,
                sequence,
            }))
            .await;
    }
}

async fn send_exit(
    transport: &TransportHandle,
    status: &ExitStatus,
    shutdown_timeout: Duration,
) -> bool {
    let (code, signal) = exit_details(status);
    let (sent_tx, sent_rx) = oneshot::channel();
    if !matches!(
        timeout(
            shutdown_timeout,
            transport.commands.send(TransportCommand::Exit {
                envelope: Envelope::Exit { code, signal },
                sent: sent_tx,
            })
        )
        .await,
        Ok(Ok(()))
    ) {
        return false;
    }
    timeout(shutdown_timeout, sent_rx).await.is_ok()
}

async fn send_shutdown_complete(
    transport: &TransportHandle,
    status: &ExitStatus,
    shutdown_timeout: Duration,
) -> bool {
    let (code, signal) = exit_details(status);
    let (sent_tx, sent_rx) = oneshot::channel();
    if !matches!(
        timeout(
            shutdown_timeout,
            transport.commands.send(TransportCommand::ShutdownComplete {
                envelope: Envelope::ShutdownComplete { code, signal },
                sent: sent_tx,
            })
        )
        .await,
        Ok(Ok(()))
    ) {
        return false;
    }
    timeout(shutdown_timeout, sent_rx).await.is_ok()
}

fn detach_transport(
    transport: &mut Option<TransportHandle>,
    deadline: &mut Option<tokio::time::Instant>,
    reconnect_grace: Duration,
) {
    if let Some(current) = transport.take() {
        current.task.abort();
    }
    if deadline.is_none() {
        *deadline = Some(tokio::time::Instant::now() + reconnect_grace);
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn new_resume_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

async fn send_envelope(socket: &mut WebSocket, envelope: Envelope) -> Result<()> {
    socket
        .send(Message::Text(envelope.to_text()?.into()))
        .await
        .map_err(|error| Error::Network(format!("cannot write WebSocket message: {error}")))
}

async fn fail_socket(socket: &mut WebSocket, code: &str, error: &Error) {
    let _ = send_envelope(
        socket,
        Envelope::Error {
            code: code.to_owned(),
            message: error.to_string(),
        },
    )
    .await;
    let _ = socket.send(Message::Close(None)).await;
}

fn error_category(error: &Error) -> &'static str {
    match error {
        Error::Config(_) => "config",
        Error::Unauthorized => "unauthorized",
        Error::Protocol(_) => "protocol",
        Error::Policy(_) => "policy",
        Error::Process(_) => "process",
        Error::Network(_) | Error::WebSocket(_) => "network",
        Error::Io(_) => "io",
        Error::Json(_) => "json",
        Error::Toml(_) => "toml",
        Error::Url(_) => "url",
        Error::Timeout(_) => "timeout",
    }
}

/// Runs the configured HTTP or HTTPS server until cancellation.
pub async fn serve(
    state: ServerState,
    insecure_listen: bool,
    shutdown: CancellationToken,
) -> Result<()> {
    let address = state.config.listen;
    let tls = state.config.tls.clone();
    if tls.is_none() && !address.ip().is_loopback() && !insecure_listen {
        return Err(Error::Config(format!(
            "refusing plaintext non-loopback listener {address}; configure TLS or pass --insecure-listen"
        )));
    }
    if state.config.allow_insecure_mcp_passthrough {
        warn!(
            "MCP passthrough is enabled; authorized clients can request remote command execution"
        );
    }

    let application = router(state.clone());
    info!(listen = %address, tls = tls.is_some(), "server listening");
    if let Some(tls) = tls {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(tls.cert_path, tls.key_path)
                .await
                .map_err(|error| {
                    Error::Config(format!("cannot load TLS configuration: {error}"))
                })?;
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        let shutdown_timeout = state.config.shutdown_timeout();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            shutdown_handle.graceful_shutdown(Some(shutdown_timeout));
        });
        axum_server::bind_rustls(address, rustls_config)
            .handle(handle)
            .serve(application.into_make_service())
            .await
            .map_err(|error| Error::Network(format!("TLS server failed: {error}")))
    } else {
        let listener = TcpListener::bind(address)
            .await
            .map_err(|error| Error::Network(format!("cannot bind {address}: {error}")))?;
        axum::serve(listener, application)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .map_err(|error| Error::Network(format!("HTTP server failed: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::auth::StaticTokenAuthenticator;

    fn test_state() -> ServerState {
        let config: ServerConfig = toml::from_str(
            r#"
            [agents.test]
            command = "test-agent"
            workspaces = ["project"]

            [workspaces.project]
            path = "/tmp"
            "#,
        )
        .unwrap();
        ServerState::new(
            Arc::new(config),
            Arc::new(StaticTokenAuthenticator::new(
                SecretToken::new("secret".into()).unwrap(),
            )),
            CancellationToken::new(),
        )
    }

    fn upgrade_request(token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .uri("/v1/tunnel")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn missing_and_incorrect_tokens_are_generic_unauthorized() {
        for token in [None, Some("wrong")] {
            let response = router(test_state())
                .oneshot(upgrade_request(token))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn correct_token_reaches_upgrade() {
        let response = router(test_state())
            .oneshot(upgrade_request(Some("secret")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn browser_origin_is_rejected_by_default() {
        let mut request = upgrade_request(Some("secret"));
        request
            .headers_mut()
            .insert(header::ORIGIN, "https://example.test".parse().unwrap());
        let response = router(test_state()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn bounded_line_codec_rejects_oversized_agent_output() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"12345\n").await.unwrap();
        drop(writer);
        let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(4));
        assert!(matches!(lines.next().await, Some(Err(_))));
    }
}
