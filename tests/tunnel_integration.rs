#![cfg(unix)]
#![doc = "Infrastructure-free end-to-end tunnel tests."]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{sync::Arc, time::Duration};

use acp_tunnel::{
    auth::StaticTokenAuthenticator,
    config::ServerConfig,
    credentials::SecretToken,
    protocol::{
        ClientEnvironment, ClientEnvironmentVariable, ClientInfo, Envelope, ResumeRequest,
        TUNNEL_VERSION,
    },
    server::{ServerState, router},
};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderValue, header::AUTHORIZATION};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

type TestSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct TestServer {
    address: std::net::SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
    _workspace: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_agent_args(&[]).await
    }

    async fn start_with_agent_args(agent_args: &[&str]) -> Self {
        let executable = std::env::var("CARGO_BIN_EXE_acp-tunnel")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_acp-tunnel").to_owned());
        let workspace = tempfile::tempdir().unwrap();
        let escaped_executable = executable.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_workspace = workspace
            .path()
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let source = format!(
            r#"
            max_frame_bytes = 1048576
            keepalive_interval_seconds = 1
            keepalive_timeout_seconds = 2
            shutdown_timeout_seconds = 1
            reconnect_grace_seconds = 2

            [agents.fake]
            command = "{escaped_executable}"
            args = ["__test-agent"]
            workspaces = ["project"]
            env = {{ SERVER_FIXED = "server-owned" }}
            client_env_allowlist = ["SESSION_CREDENTIAL", "SERVER_FIXED"]
            mcp_policy = "deny"

            [workspaces.project]
            path = "{escaped_workspace}"
            "#
        );
        let mut config: ServerConfig = toml::from_str(&source).unwrap();
        config
            .agents
            .get_mut("fake")
            .unwrap()
            .args
            .extend(agent_args.iter().map(|argument| (*argument).to_owned()));
        config.validate().unwrap();
        let shutdown = CancellationToken::new();
        let state = ServerState::new(
            Arc::new(config),
            Arc::new(StaticTokenAuthenticator::new(
                SecretToken::new("integration-secret".into()).unwrap(),
            )),
            shutdown.clone(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router(state))
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await;
        });
        Self {
            address,
            shutdown,
            task,
            _workspace: workspace,
        }
    }

    async fn connect(&self) -> TestSocket {
        self.connect_with_resume(None).await.0
    }

    async fn authenticated_socket(&self) -> TestSocket {
        let mut request = format!("ws://{}/v1/tunnel", self.address)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer integration-secret"),
        );
        connect_async(request).await.unwrap().0
    }

    async fn connect_with_resume(
        &self,
        resume: Option<ResumeRequest>,
    ) -> (TestSocket, ResumeRequest) {
        self.connect_with_resume_and_environment(resume, ClientEnvironment::default())
            .await
    }

    async fn connect_with_resume_and_environment(
        &self,
        resume: Option<ResumeRequest>,
        client_environment: ClientEnvironment,
    ) -> (TestSocket, ResumeRequest) {
        let mut socket = self.authenticated_socket().await;
        send(
            &mut socket,
            Envelope::Open {
                tunnel_version: TUNNEL_VERSION,
                agent: "fake".into(),
                workspace: "project".into(),
                client_info: ClientInfo {
                    name: "integration-test".into(),
                    version: "0".into(),
                },
                client_environment,
                resume,
            },
        )
        .await;
        let ready = receive_raw(&mut socket).await;
        let Envelope::Ready {
            connection_id,
            resume_token: Some(resume_token),
            ..
        } = ready
        else {
            panic!("expected resumable ready envelope: {ready:?}");
        };
        (
            socket,
            ResumeRequest {
                connection_id,
                resume_token,
            },
        )
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.task).await;
    }
}

async fn receive_shutdown_complete(socket: &mut TestSocket) -> (Option<i32>, Option<i32>) {
    loop {
        match receive_raw(socket).await {
            Envelope::ShutdownComplete { code, signal } => return (code, signal),
            Envelope::Ping { nonce } => send(socket, Envelope::Pong { nonce }).await,
            Envelope::Stderr { .. }
            | Envelope::Pong { .. }
            | Envelope::Ack { .. }
            | Envelope::Acp { .. } => {}
            other => panic!("unexpected envelope before shutdown completion: {other:?}"),
        }
    }
}

async fn assert_resume_rejected(server: &TestServer, resume: ResumeRequest) {
    let mut socket = server.authenticated_socket().await;
    send(
        &mut socket,
        Envelope::Open {
            tunnel_version: TUNNEL_VERSION,
            agent: "fake".into(),
            workspace: "project".into(),
            client_info: ClientInfo {
                name: "integration-test".into(),
                version: "0".into(),
            },
            client_environment: ClientEnvironment::default(),
            resume: Some(resume),
        },
    )
    .await;
    match receive_raw(&mut socket).await {
        Envelope::Error { code, .. } => assert_eq!(code, "resume_rejected"),
        other => panic!("expected resume rejection, got {other:?}"),
    }
}

async fn send(socket: &mut TestSocket, envelope: Envelope) {
    socket
        .send(Message::Text(envelope.to_text().unwrap().into()))
        .await
        .unwrap();
}

async fn send_acp(socket: &mut TestSocket, sequence: u64, value: Value) {
    send(
        socket,
        Envelope::Acp {
            sequence: Some(sequence),
            payload: serde_json::to_string(&value).unwrap(),
        },
    )
    .await;
}

async fn receive(socket: &mut TestSocket) -> Envelope {
    let envelope = receive_raw(socket).await;
    if let Envelope::Acp {
        sequence: Some(sequence),
        ..
    } = &envelope
    {
        send(
            socket,
            Envelope::Ack {
                stream: acp_tunnel::protocol::AckStream::ServerToClient,
                sequence: *sequence,
            },
        )
        .await;
    }
    envelope
}

async fn receive_raw(socket: &mut TestSocket) -> Envelope {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match message {
            Message::Text(text) => return Envelope::from_text(&text).unwrap(),
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await.unwrap();
            }
            Message::Pong(_) => {}
            other => panic!("unexpected WebSocket message: {other:?}"),
        }
    }
}

async fn receive_acp_with_id(socket: &mut TestSocket, expected_id: &str) -> Value {
    loop {
        match receive(socket).await {
            Envelope::Acp { payload, .. } => {
                let value: Value = serde_json::from_str(&payload).unwrap();
                if value.get("id").and_then(Value::as_str) == Some(expected_id) {
                    return value;
                }
            }
            Envelope::Ping { nonce } => send(socket, Envelope::Pong { nonce }).await,
            Envelope::Stderr { .. } | Envelope::Pong { .. } | Envelope::Ack { .. } => {}
            other => panic!("unexpected envelope while waiting for ACP: {other:?}"),
        }
    }
}

#[tokio::test]
async fn full_fake_agent_flow_is_bidirectional_and_propagates_exit() {
    let server = TestServer::start().await;
    let mut socket = server.connect().await;

    send_acp(
        &mut socket,
        1,
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}),
    )
    .await;
    let initialize = receive_acp_with_id(&mut socket, "init").await;
    assert_eq!(
        initialize["result"]["agentInfo"]["name"],
        "acp-tunnel-test-agent"
    );

    send_acp(
        &mut socket,
        2,
        json!({
            "jsonrpc":"2.0",
            "id":"new",
            "method":"session/new",
            "params":{
                "cwd":"/local/does-not-exist",
                "mcpServers":[{"name":"evil","command":"evil"}],
                "_meta":{"unknown":{"survives":true}}
            }
        }),
    )
    .await;
    let new_session = receive_acp_with_id(&mut socket, "new").await;
    assert_ne!(
        new_session["result"]["observedCwd"],
        "/local/does-not-exist"
    );

    send_acp(
        &mut socket,
        3,
        json!({
            "jsonrpc":"2.0",
            "id":"prompt",
            "method":"session/prompt",
            "params":{"sessionId":"test-session","prompt":[]}
        }),
    )
    .await;
    let mut saw_update = false;
    let mut saw_permission = false;
    let mut saw_prompt_result = false;
    while !(saw_update && saw_permission && saw_prompt_result) {
        match receive(&mut socket).await {
            Envelope::Acp { payload, .. } => {
                let value: Value = serde_json::from_str(&payload).unwrap();
                match (
                    value.get("method").and_then(Value::as_str),
                    value.get("id").and_then(Value::as_str),
                ) {
                    (Some("session/update"), _) => saw_update = true,
                    (Some("session/request_permission"), Some("agent-permission-1")) => {
                        saw_permission = true;
                        send_acp(
                            &mut socket,
                            4,
                            json!({
                                "jsonrpc":"2.0",
                                "id":"agent-permission-1",
                                "result":{"outcome":"cancelled"}
                            }),
                        )
                        .await;
                    }
                    (_, Some("prompt")) => saw_prompt_result = true,
                    _ => {}
                }
            }
            Envelope::Ping { nonce } => send(&mut socket, Envelope::Pong { nonce }).await,
            Envelope::Stderr { .. } | Envelope::Pong { .. } | Envelope::Ack { .. } => {}
            other => panic!("unexpected envelope: {other:?}"),
        }
    }

    send_acp(
        &mut socket,
        5,
        json!({"jsonrpc":"2.0","id":"stderr","method":"test/stderr","params":{}}),
    )
    .await;
    let stderr_result = receive_acp_with_id(&mut socket, "stderr").await;
    assert_eq!(stderr_result["result"]["stderrComplete"], true);

    send_acp(
        &mut socket,
        6,
        json!({
            "jsonrpc":"2.0",
            "method":"session/cancel",
            "params":{"sessionId":"test-session"}
        }),
    )
    .await;
    send_acp(
        &mut socket,
        7,
        json!({"jsonrpc":"2.0","id":"exit","method":"test/exit","params":{}}),
    )
    .await;
    let _ = receive_acp_with_id(&mut socket, "exit").await;
    loop {
        match receive(&mut socket).await {
            Envelope::Exit { code, signal } => {
                assert_eq!(code, Some(0));
                assert_eq!(signal, None);
                break;
            }
            Envelope::Ping { nonce } => send(&mut socket, Envelope::Pong { nonce }).await,
            Envelope::Stderr { .. } | Envelope::Pong { .. } | Envelope::Ack { .. } => {}
            other => panic!("unexpected envelope before exit: {other:?}"),
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn explicitly_selected_agent_environment_is_allowlisted_and_server_owned() {
    let server = TestServer::start().await;
    let client_environment = ClientEnvironment::new(vec![
        ClientEnvironmentVariable::new(
            "SESSION_CREDENTIAL".into(),
            "client-selected-secret".into(),
        ),
        ClientEnvironmentVariable::new("SERVER_FIXED".into(), "client-override".into()),
    ]);
    let (mut socket, _) = server
        .connect_with_resume_and_environment(None, client_environment)
        .await;

    for (sequence, name, expected) in [
        (1, "SESSION_CREDENTIAL", Some("client-selected-secret")),
        (2, "SERVER_FIXED", Some("server-owned")),
        (3, "USER", None),
    ] {
        let id = format!("environment-{sequence}");
        send_acp(
            &mut socket,
            sequence,
            json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":"test/environment",
                "params":{"name":name}
            }),
        )
        .await;
        let response = receive_acp_with_id(&mut socket, &id).await;
        assert_eq!(response["result"]["value"].as_str(), expected);
    }

    send(
        &mut socket,
        Envelope::Shutdown {
            reason: acp_tunnel::protocol::ShutdownReason::ClientShutdown,
        },
    )
    .await;
    let _ = receive_shutdown_complete(&mut socket).await;
    server.stop().await;
}

#[tokio::test]
async fn unlisted_agent_environment_is_rejected_without_disclosing_values() {
    let server = TestServer::start().await;
    let mut socket = server.authenticated_socket().await;
    send(
        &mut socket,
        Envelope::Open {
            tunnel_version: TUNNEL_VERSION,
            agent: "fake".into(),
            workspace: "project".into(),
            client_info: ClientInfo {
                name: "integration-test".into(),
                version: "0".into(),
            },
            client_environment: ClientEnvironment::new(vec![ClientEnvironmentVariable::new(
                "UNLISTED".into(),
                "do-not-disclose".into(),
            )]),
            resume: None,
        },
    )
    .await;
    match receive_raw(&mut socket).await {
        Envelope::Error { code, message } => {
            assert_eq!(code, "client_environment_rejected");
            assert!(!message.contains("do-not-disclose"));
        }
        other => panic!("expected client environment rejection, got {other:?}"),
    }
    server.stop().await;
}

#[tokio::test]
async fn transport_disconnect_preserves_child_for_authenticated_resume() {
    let server = TestServer::start().await;
    let (mut first, resume) = server.connect_with_resume(None).await;
    let mut second = server.connect().await;

    let pid = loop {
        match receive(&mut first).await {
            Envelope::Stderr { payload } if payload.starts_with("fake-agent pid=") => {
                break payload["fake-agent pid=".len()..].parse::<i32>().unwrap();
            }
            Envelope::Ping { nonce } => send(&mut first, Envelope::Pong { nonce }).await,
            _ => {}
        }
    };
    first.close(None).await.unwrap();
    assert!(kill(Pid::from_raw(pid), None).is_ok());
    let (mut resumed, returned_resume) = server.connect_with_resume(Some(resume.clone())).await;
    assert_eq!(returned_resume, resume);
    send_acp(
        &mut resumed,
        1,
        json!({"jsonrpc":"2.0","id":"pid","method":"test/pid","params":{}}),
    )
    .await;
    let pid_response = receive_acp_with_id(&mut resumed, "pid").await;
    assert_eq!(pid_response["result"]["pid"], pid);
    resumed.close(None).await.unwrap();
    second.close(None).await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn resume_replays_unacknowledged_frames_without_redelivering_client_input() {
    let server = TestServer::start().await;
    let (mut socket, resume) = server.connect_with_resume(None).await;
    send_acp(
        &mut socket,
        1,
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}),
    )
    .await;

    let original_payload = loop {
        match receive_raw(&mut socket).await {
            Envelope::Acp {
                sequence: Some(1),
                payload,
            } => break payload,
            Envelope::Ping { nonce } => send(&mut socket, Envelope::Pong { nonce }).await,
            Envelope::Ack { .. } | Envelope::Stderr { .. } | Envelope::Pong { .. } => {}
            other => panic!("unexpected envelope before disconnect: {other:?}"),
        }
    };
    socket.close(None).await.unwrap();

    let (mut resumed, returned_resume) = server.connect_with_resume(Some(resume.clone())).await;
    assert_eq!(returned_resume, resume);
    loop {
        match receive_raw(&mut resumed).await {
            Envelope::Acp {
                sequence: Some(1),
                payload,
            } => {
                assert_eq!(payload, original_payload);
                send(
                    &mut resumed,
                    Envelope::Ack {
                        stream: acp_tunnel::protocol::AckStream::ServerToClient,
                        sequence: 1,
                    },
                )
                .await;
                break;
            }
            Envelope::Ping { nonce } => send(&mut resumed, Envelope::Pong { nonce }).await,
            Envelope::Ack { .. } | Envelope::Stderr { .. } | Envelope::Pong { .. } => {}
            other => panic!("unexpected envelope during replay: {other:?}"),
        }
    }

    send_acp(
        &mut resumed,
        1,
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}),
    )
    .await;
    send_acp(
        &mut resumed,
        2,
        json!({
            "jsonrpc":"2.0",
            "id":"new-after-resume",
            "method":"session/new",
            "params":{"cwd":"/local","mcpServers":[]}
        }),
    )
    .await;
    let mut duplicate_init_response = false;
    loop {
        match receive(&mut resumed).await {
            Envelope::Acp { payload, .. } => {
                let value: Value = serde_json::from_str(&payload).unwrap();
                match value.get("id").and_then(Value::as_str) {
                    Some("init") => duplicate_init_response = true,
                    Some("new-after-resume") => break,
                    _ => {}
                }
            }
            Envelope::Ping { nonce } => send(&mut resumed, Envelope::Pong { nonce }).await,
            Envelope::Ack { .. } | Envelope::Stderr { .. } | Envelope::Pong { .. } => {}
            other => panic!("unexpected envelope after replay: {other:?}"),
        }
    }
    assert!(!duplicate_init_response);
    resumed.close(None).await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn invalid_resume_capability_is_rejected_without_disrupting_the_session() {
    let server = TestServer::start().await;
    let (mut initial, resume) = server.connect_with_resume(None).await;
    initial.close(None).await.unwrap();

    let mut changed_environment = server.authenticated_socket().await;
    send(
        &mut changed_environment,
        Envelope::Open {
            tunnel_version: TUNNEL_VERSION,
            agent: "fake".into(),
            workspace: "project".into(),
            client_info: ClientInfo {
                name: "integration-test".into(),
                version: "0".into(),
            },
            client_environment: ClientEnvironment::new(vec![ClientEnvironmentVariable::new(
                "SESSION_CREDENTIAL".into(),
                "replacement-secret".into(),
            )]),
            resume: Some(resume.clone()),
        },
    )
    .await;
    match receive_raw(&mut changed_environment).await {
        Envelope::Error { code, message } => {
            assert_eq!(code, "resume_rejected");
            assert!(!message.contains("replacement-secret"));
        }
        other => panic!("expected environment-changing resume rejection, got {other:?}"),
    }

    let mut invalid = server.authenticated_socket().await;
    let mut bad_resume = resume.clone();
    bad_resume.resume_token.push_str("-incorrect");
    send(
        &mut invalid,
        Envelope::Open {
            tunnel_version: TUNNEL_VERSION,
            agent: "fake".into(),
            workspace: "project".into(),
            client_info: ClientInfo {
                name: "integration-test".into(),
                version: "0".into(),
            },
            client_environment: ClientEnvironment::default(),
            resume: Some(bad_resume),
        },
    )
    .await;
    match receive_raw(&mut invalid).await {
        Envelope::Error { code, message } => {
            assert_eq!(code, "resume_rejected");
            assert_eq!(message, "protocol error: resume request was rejected");
        }
        other => panic!("expected generic resume rejection, got {other:?}"),
    }

    let (mut resumed, returned_resume) = server.connect_with_resume(Some(resume.clone())).await;
    assert_eq!(returned_resume, resume);
    send_acp(
        &mut resumed,
        1,
        json!({"jsonrpc":"2.0","id":"pid","method":"test/pid","params":{}}),
    )
    .await;
    let response = receive_acp_with_id(&mut resumed, "pid").await;
    assert!(response["result"]["pid"].as_i64().is_some());
    resumed.close(None).await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn tunnel_protocol_version_two_is_rejected_clearly() {
    let server = TestServer::start().await;
    let mut socket = server.authenticated_socket().await;
    send(
        &mut socket,
        Envelope::Open {
            tunnel_version: 2,
            agent: "fake".into(),
            workspace: "project".into(),
            client_info: ClientInfo {
                name: "integration-test".into(),
                version: "0".into(),
            },
            client_environment: ClientEnvironment::default(),
            resume: None,
        },
    )
    .await;
    match receive_raw(&mut socket).await {
        Envelope::Error { code, message } => {
            assert_eq!(code, "unsupported_tunnel_version");
            assert!(message.contains("unsupported tunnel version 2"));
            assert!(message.contains("expected 3"));
        }
        other => panic!("expected version rejection, got {other:?}"),
    }
    server.stop().await;
}

#[tokio::test]
async fn connect_command_keeps_stdout_protocol_pure() {
    let server = TestServer::start().await;
    let executable = std::env::var("CARGO_BIN_EXE_acp-tunnel")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_acp-tunnel").to_owned());
    let mut child = Command::new(executable)
        .args([
            "connect",
            "--url",
            &format!("ws://{}/v1/tunnel", server.address),
            "--agent",
            "fake",
            "--workspace",
            "project",
            "--client-env",
            "SESSION_CREDENTIAL",
            "--shutdown-timeout-seconds",
            "3",
        ])
        .env("ACP_TUNNEL_TOKEN", "integration-secret")
        .env("SESSION_CREDENTIAL", "cli-selected-secret")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":"client-init","method":"initialize","params":{}}
{"jsonrpc":"2.0","id":"client-environment","method":"test/environment","params":{"name":"SESSION_CREDENTIAL"}}
"#,
        )
        .await
        .unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let message: Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(message["id"], "client-init");
    assert_eq!(
        message["result"]["agentInfo"]["name"],
        "acp-tunnel-test-agent"
    );
    line.clear();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let environment: Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(environment["id"], "client-environment");
    assert_eq!(
        environment["result"]["value"],
        Value::String("cli-selected-secret".into())
    );
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    server.stop().await;
}

#[tokio::test]
async fn keepalive_timeout_terminates_an_unresponsive_tunnel() {
    let server = TestServer::start().await;
    let mut socket = server.connect().await;
    let pid = loop {
        match receive(&mut socket).await {
            Envelope::Stderr { payload } if payload.starts_with("fake-agent pid=") => {
                break payload["fake-agent pid=".len()..].parse::<i32>().unwrap();
            }
            Envelope::Ping { nonce } => send(&mut socket, Envelope::Pong { nonce }).await,
            _ => {}
        }
    };
    tokio::time::sleep(Duration::from_secs(6)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if kill(Pid::from_raw(pid), None).is_err() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "child process {pid} survived reconnect grace expiration"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    server.stop().await;
}

#[tokio::test]
async fn server_shutdown_cleans_up_the_active_child() {
    let server = TestServer::start().await;
    let mut socket = server.connect().await;
    let pid = loop {
        match receive(&mut socket).await {
            Envelope::Stderr { payload } if payload.starts_with("fake-agent pid=") => {
                break payload["fake-agent pid=".len()..].parse::<i32>().unwrap();
            }
            Envelope::Ping { nonce } => send(&mut socket, Envelope::Pong { nonce }).await,
            _ => {}
        }
    };
    server.shutdown.cancel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if kill(Pid::from_raw(pid), None).is_err() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "child process {pid} survived server shutdown"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    server.stop().await;
}

#[tokio::test]
async fn explicit_shutdown_closes_stdin_and_allows_a_cooperative_exit() {
    let server = TestServer::start().await;
    let (mut socket, resume) = server.connect_with_resume(None).await;
    send(
        &mut socket,
        Envelope::Shutdown {
            reason: acp_tunnel::protocol::ShutdownReason::ClientShutdown,
        },
    )
    .await;
    let (code, signal) = receive_shutdown_complete(&mut socket).await;
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);
    assert_resume_rejected(&server, resume).await;
    server.stop().await;
}

#[tokio::test]
async fn shutdown_invalidates_resume_before_escalation_and_is_idempotent() {
    let server = TestServer::start_with_agent_args(&["--uncooperative"]).await;
    let (mut socket, resume) = server.connect_with_resume(None).await;
    let shutdown = Envelope::Shutdown {
        reason: acp_tunnel::protocol::ShutdownReason::ClientShutdown,
    };
    send(&mut socket, shutdown.clone()).await;
    send(&mut socket, shutdown).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_resume_rejected(&server, resume).await;
    let (code, signal) = receive_shutdown_complete(&mut socket).await;
    assert_eq!(code, None);
    assert_eq!(signal, Some(Signal::SIGKILL as i32));
    server.stop().await;
}

#[tokio::test]
async fn child_exit_racing_with_shutdown_returns_shutdown_complete() {
    let server = TestServer::start().await;
    let mut socket = server.connect().await;
    send_acp(
        &mut socket,
        1,
        json!({"jsonrpc":"2.0","id":"exit","method":"test/exit","params":{}}),
    )
    .await;
    send(
        &mut socket,
        Envelope::Shutdown {
            reason: acp_tunnel::protocol::ShutdownReason::ClientShutdown,
        },
    )
    .await;
    let (code, signal) = receive_shutdown_complete(&mut socket).await;
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);
    server.stop().await;
}

#[tokio::test]
async fn explicit_shutdown_cleans_up_the_complete_process_group() {
    let server = TestServer::start_with_agent_args(&["--spawn-grandchild"]).await;
    let mut socket = server.connect().await;
    let (parent_pid, grandchild_pid) = {
        let mut parent = None;
        let mut grandchild = None;
        while parent.is_none() || grandchild.is_none() {
            match receive(&mut socket).await {
                Envelope::Stderr { payload } if payload.starts_with("fake-agent pid=") => {
                    parent = Some(payload["fake-agent pid=".len()..].parse::<i32>().unwrap());
                }
                Envelope::Stderr { payload }
                    if payload.starts_with("fake-agent grandchild-pid=") =>
                {
                    grandchild = Some(
                        payload["fake-agent grandchild-pid=".len()..]
                            .parse::<i32>()
                            .unwrap(),
                    );
                }
                Envelope::Ping { nonce } => send(&mut socket, Envelope::Pong { nonce }).await,
                _ => {}
            }
        }
        (parent.unwrap(), grandchild.unwrap())
    };
    send(
        &mut socket,
        Envelope::Shutdown {
            reason: acp_tunnel::protocol::ShutdownReason::ClientShutdown,
        },
    )
    .await;
    let (code, signal) = receive_shutdown_complete(&mut socket).await;
    assert_eq!(code, Some(0));
    assert_eq!(signal, None);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    for pid in [parent_pid, grandchild_pid] {
        while kill(Pid::from_raw(pid), None).is_ok() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "process {pid} survived explicit process-group cleanup"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    server.stop().await;
}

async fn connector_signal_requests_shutdown(signal: Signal) {
    let server = TestServer::start().await;
    let executable = std::env::var("CARGO_BIN_EXE_acp-tunnel")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_acp-tunnel").to_owned());
    let mut child = Command::new(executable)
        .args([
            "connect",
            "--url",
            &format!("ws://{}/v1/tunnel", server.address),
            "--agent",
            "fake",
            "--workspace",
            "project",
            "--shutdown-timeout-seconds",
            "3",
        ])
        .env("ACP_TUNNEL_TOKEN", "integration-secret")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":"ready","method":"initialize","params":{}}
"#,
        )
        .await
        .unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(line.trim_end()).unwrap()["id"],
        "ready"
    );

    let pid = i32::try_from(child.id().unwrap()).unwrap();
    kill(Pid::from_raw(pid), signal).unwrap();
    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
            let _ = child.wait().await;
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_string(&mut stderr).await.unwrap();
            }
            panic!("connector did not stop after {signal:?}: {stderr}");
        }
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).await.unwrap();
    }
    assert!(
        status.success(),
        "connector failed after {signal:?}: {stderr}"
    );
    drop(stdin);
    server.stop().await;
}

#[tokio::test]
async fn sigterm_uses_the_explicit_shutdown_path() {
    connector_signal_requests_shutdown(Signal::SIGTERM).await;
}

#[tokio::test]
async fn interrupt_uses_the_explicit_shutdown_path() {
    connector_signal_requests_shutdown(Signal::SIGINT).await;
}
