use std::{
    collections::VecDeque,
    net::{IpAddr, ToSocketAddrs},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use http::{HeaderValue, header::AUTHORIZATION};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::TcpStream,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        protocol::{WebSocketConfig, frame::coding::CloseCode},
    },
};
use url::{Host, Url};
use uuid::Uuid;

use crate::{
    Error, Result,
    config::validate_id,
    protocol::{AckStream, ClientInfo, Envelope, ResumeRequest, TUNNEL_VERSION},
};

type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Runtime settings for `acp-tunnel connect`.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Tunnel WebSocket URL.
    pub url: Url,
    /// Requested configured agent identifier.
    pub agent: String,
    /// Requested configured workspace identifier.
    pub workspace: String,
    /// Bearer token read from the environment.
    pub token: String,
    /// Maximum ACP line and WebSocket message size.
    pub max_frame_bytes: usize,
    /// Maximum locally retained unacknowledged ACP frames.
    pub max_replay_frames: usize,
    /// Maximum locally retained unacknowledged ACP payload bytes.
    pub max_replay_bytes: usize,
    /// Connection and opening handshake timeout.
    pub connection_timeout: Duration,
    /// Keepalive timeout.
    pub keepalive_timeout: Duration,
    /// Maximum time spent reconnecting one detached tunnel.
    pub reconnect_timeout: Duration,
}

#[derive(Clone)]
struct ResumeCredentials {
    connection_id: String,
    resume_token: String,
}

struct PendingFrame {
    sequence: u64,
    payload: String,
}

/// Validates a client URL, including the non-loopback TLS requirement.
pub fn validate_connect_url(url: &Url) -> Result<()> {
    match url.scheme() {
        "wss" => Ok(()),
        "ws" if host_is_loopback(url)? => Ok(()),
        "ws" => Err(Error::Config(
            "ws:// is allowed only for loopback destinations; use wss://".into(),
        )),
        scheme => Err(Error::Config(format!(
            "unsupported URL scheme {scheme:?}; expected wss:// or loopback ws://"
        ))),
    }
}

/// Runs the stdio-to-WebSocket client. This function never writes to stdout
/// except for remote ACP payload lines.
pub async fn connect(options: ConnectOptions) -> Result<()> {
    validate_options(&options)?;
    let (mut socket, credentials) = open_socket(&options, None).await?;
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = BufWriter::new(tokio::io::stdout());
    let mut pending = VecDeque::<PendingFrame>::new();
    let mut pending_bytes = 0_usize;
    let mut next_client_sequence = 1_u64;
    let mut expected_server_sequence = 1_u64;
    let mut last_received = Instant::now();

    loop {
        let can_read_stdin = pending.len() < options.max_replay_frames
            && pending_bytes
                <= options
                    .max_replay_bytes
                    .saturating_sub(options.max_frame_bytes);
        tokio::select! {
            line = lines.next_line(), if can_read_stdin => {
                match line? {
                    Some(line) => {
                        if line.len() > options.max_frame_bytes {
                            return Err(Error::Protocol(format!(
                                "local ACP line exceeds {} bytes",
                                options.max_frame_bytes
                            )));
                        }
                        if pending_bytes.saturating_add(line.len()) > options.max_replay_bytes {
                            return Err(Error::Protocol(
                                "local replay byte limit is smaller than one ACP frame".into(),
                            ));
                        }
                        let sequence = next_client_sequence;
                        next_client_sequence = next_client_sequence.checked_add(1)
                            .ok_or_else(|| Error::Protocol("client ACP sequence exhausted".into()))?;
                        pending_bytes = pending_bytes.saturating_add(line.len());
                        pending.push_back(PendingFrame {
                            sequence,
                            payload: line,
                        });
                        let frame = pending.back().ok_or_else(|| {
                            Error::Protocol("client replay queue unexpectedly empty".into())
                        })?;
                        if send_acp(&mut socket, frame).await.is_err() {
                            socket = reconnect_and_replay(
                                &options,
                                &credentials,
                                &pending,
                            ).await?;
                            last_received = Instant::now();
                        }
                    }
                    None => {
                        let _ = socket.send(Message::Close(Some(
                            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                code: CloseCode::Normal,
                                reason: "local stdin closed".into(),
                            },
                        ))).await;
                        return Ok(());
                    }
                }
            }
            message = socket.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(_)) | None => {
                        socket = reconnect_and_replay(&options, &credentials, &pending).await?;
                        last_received = Instant::now();
                        continue;
                    }
                };
                last_received = Instant::now();
                match message {
                    Message::Text(text) => match Envelope::from_text(&text)? {
                        Envelope::Acp { sequence: Some(sequence), payload } => {
                            if payload.len() > options.max_frame_bytes {
                                return Err(Error::Protocol(
                                    "remote ACP line exceeds configured limit".into(),
                                ));
                            }
                            if sequence == expected_server_sequence {
                                stdout.write_all(payload.as_bytes()).await?;
                                stdout.write_all(b"\n").await?;
                                stdout.flush().await?;
                                expected_server_sequence = expected_server_sequence
                                    .checked_add(1)
                                    .ok_or_else(|| Error::Protocol(
                                        "server ACP sequence exhausted".into()
                                    ))?;
                            } else if sequence > expected_server_sequence {
                                return Err(Error::Protocol(format!(
                                    "server ACP sequence gap: expected {expected_server_sequence}, received {sequence}"
                                )));
                            }
                            let acknowledged = expected_server_sequence.saturating_sub(1);
                            if socket.send(Message::Text(Envelope::Ack {
                                stream: AckStream::ServerToClient,
                                sequence: acknowledged,
                            }.to_text()?.into())).await.is_err() {
                                socket = reconnect_and_replay(
                                    &options,
                                    &credentials,
                                    &pending,
                                ).await?;
                                last_received = Instant::now();
                            }
                        }
                        Envelope::Acp { sequence: None, .. } => {
                            return Err(Error::Protocol(
                                "tunnel v2 ACP frame is missing a sequence number".into(),
                            ));
                        }
                        Envelope::Ack {
                            stream: AckStream::ClientToServer,
                            sequence,
                        } => {
                            if sequence >= next_client_sequence {
                                return Err(Error::Protocol(format!(
                                    "server acknowledged unsent client sequence {sequence}"
                                )));
                            }
                            while pending.front().is_some_and(|frame| frame.sequence <= sequence) {
                                if let Some(frame) = pending.pop_front() {
                                    pending_bytes = pending_bytes.saturating_sub(frame.payload.len());
                                }
                            }
                        }
                        Envelope::Ack { .. } => {
                            return Err(Error::Protocol(
                                "server sent an acknowledgement for the wrong stream".into(),
                            ));
                        }
                        Envelope::Stderr { payload } => {
                            let mut stderr = tokio::io::stderr();
                            stderr.write_all(payload.as_bytes()).await?;
                            stderr.write_all(b"\n").await?;
                            stderr.flush().await?;
                        }
                        Envelope::Ping { nonce } => {
                            if socket.send(Message::Text(
                                Envelope::Pong { nonce }.to_text()?.into()
                            )).await.is_err() {
                                socket = reconnect_and_replay(
                                    &options,
                                    &credentials,
                                    &pending,
                                ).await?;
                                last_received = Instant::now();
                            }
                        }
                        Envelope::Pong { .. } => {}
                        Envelope::Exit { code, signal } => {
                            if code == Some(0) {
                                return Ok(());
                            }
                            return Err(Error::Process(format!(
                                "remote agent exited with code {code:?}, signal {signal:?}"
                            )));
                        }
                        Envelope::Error { code, message } => {
                            return Err(Error::Protocol(format!("{code}: {message}")));
                        }
                        Envelope::Open { .. } | Envelope::Ready { .. } => {
                            return Err(Error::Protocol("unexpected handshake envelope".into()));
                        }
                    },
                    Message::Ping(payload) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            socket = reconnect_and_replay(
                                &options,
                                &credentials,
                                &pending,
                            ).await?;
                            last_received = Instant::now();
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => {
                        socket = reconnect_and_replay(&options, &credentials, &pending).await?;
                        last_received = Instant::now();
                    }
                    Message::Binary(_) | Message::Frame(_) => {
                        return Err(Error::Protocol(
                            "binary WebSocket messages are not supported".into(),
                        ));
                    }
                }
            }
            () = tokio::time::sleep_until(last_received + options.keepalive_timeout) => {
                let _ = socket.close(None).await;
                socket = reconnect_and_replay(&options, &credentials, &pending).await?;
                last_received = Instant::now();
            }
        }
    }
}

fn validate_options(options: &ConnectOptions) -> Result<()> {
    validate_id("agent", &options.agent)?;
    validate_id("workspace", &options.workspace)?;
    validate_connect_url(&options.url)?;
    if options.max_frame_bytes == 0
        || options.max_replay_frames == 0
        || options.max_replay_bytes < options.max_frame_bytes
        || options.connection_timeout.is_zero()
        || options.keepalive_timeout.is_zero()
        || options.reconnect_timeout.is_zero()
    {
        return Err(Error::Config(
            "client frame, replay, and timeout settings are invalid".into(),
        ));
    }
    Ok(())
}

async fn open_socket(
    options: &ConnectOptions,
    resume: Option<&ResumeCredentials>,
) -> Result<(ClientSocket, ResumeCredentials)> {
    let authorization = HeaderValue::from_str(&format!("Bearer {}", options.token))
        .map_err(|_| Error::Config("token cannot be represented as an HTTP header".into()))?;
    let mut request = options
        .url
        .as_str()
        .into_client_request()
        .map_err(|error| Error::Network(format!("cannot build WebSocket request: {error}")))?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    request.headers_mut().insert(
        "User-Agent",
        HeaderValue::from_str(&format!("acp-tunnel/{}", env!("CARGO_PKG_VERSION")))
            .map_err(|_| Error::Config("invalid client version header".into()))?,
    );
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(options.max_frame_bytes))
        .max_frame_size(Some(options.max_frame_bytes));
    let (mut socket, _response) = timeout(
        options.connection_timeout,
        connect_async_tls_with_config(request, Some(websocket_config), true, None),
    )
    .await
    .map_err(|_| Error::Timeout("connecting to tunnel server"))??;

    socket
        .send(Message::Text(
            Envelope::Open {
                tunnel_version: TUNNEL_VERSION,
                agent: options.agent.clone(),
                workspace: options.workspace.clone(),
                client_info: ClientInfo {
                    name: "acp-tunnel".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
                resume: resume.map(|credentials| ResumeRequest {
                    connection_id: credentials.connection_id.clone(),
                    resume_token: credentials.resume_token.clone(),
                }),
            }
            .to_text()?
            .into(),
        ))
        .await?;

    let ready_message = timeout(options.connection_timeout, socket.next())
        .await
        .map_err(|_| Error::Timeout("waiting for tunnel ready response"))?
        .ok_or_else(|| Error::Network("server closed before ready response".into()))??;
    match ready_message {
        Message::Text(text) => match Envelope::from_text(&text)? {
            Envelope::Ready {
                tunnel_version,
                connection_id,
                resume_token: Some(resume_token),
                resumed,
            } if tunnel_version == TUNNEL_VERSION && resumed == resume.is_some() => Ok((
                socket,
                ResumeCredentials {
                    connection_id,
                    resume_token,
                },
            )),
            Envelope::Error { code, message } => Err(Error::Protocol(format!("{code}: {message}"))),
            _ => Err(Error::Protocol("expected tunnel v2 ready response".into())),
        },
        _ => Err(Error::Protocol("expected text ready response".into())),
    }
}

async fn reconnect_and_replay(
    options: &ConnectOptions,
    credentials: &ResumeCredentials,
    pending: &VecDeque<PendingFrame>,
) -> Result<ClientSocket> {
    let deadline = Instant::now() + options.reconnect_timeout;
    let mut delay = Duration::from_millis(200);
    loop {
        match open_socket(options, Some(credentials)).await {
            Ok((mut socket, returned_credentials))
                if returned_credentials.connection_id == credentials.connection_id
                    && returned_credentials.resume_token == credentials.resume_token =>
            {
                let mut replay_failed = false;
                for frame in pending {
                    if send_acp(&mut socket, frame).await.is_err() {
                        replay_failed = true;
                        break;
                    }
                }
                if !replay_failed {
                    write_diagnostic("reconnected to remote ACP agent").await;
                    return Ok(socket);
                }
            }
            Ok(_) => {
                return Err(Error::Protocol(
                    "server changed resume credentials during reconnect".into(),
                ));
            }
            Err(error) => {
                write_diagnostic(&format!("reconnect attempt failed: {error}")).await;
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout("reconnecting detached tunnel"));
        }
        tokio::time::sleep(delay.min(deadline.saturating_duration_since(Instant::now()))).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

async fn send_acp(socket: &mut ClientSocket, frame: &PendingFrame) -> Result<()> {
    socket
        .send(Message::Text(
            Envelope::Acp {
                sequence: Some(frame.sequence),
                payload: frame.payload.clone(),
            }
            .to_text()?
            .into(),
        ))
        .await?;
    Ok(())
}

async fn write_diagnostic(message: &str) {
    let mut stderr = tokio::io::stderr();
    let _ = stderr.write_all(b"acp-tunnel: ").await;
    let _ = stderr.write_all(message.as_bytes()).await;
    let _ = stderr.write_all(b"\n").await;
    let _ = stderr.flush().await;
}

fn host_is_loopback(url: &Url) -> Result<bool> {
    match url.host() {
        Some(Host::Ipv4(address)) => Ok(address.is_loopback()),
        Some(Host::Ipv6(address)) => Ok(address.is_loopback()),
        Some(Host::Domain(name)) if name.eq_ignore_ascii_case("localhost") => Ok(true),
        Some(Host::Domain(name)) => {
            let port = url.port_or_known_default().unwrap_or(80);
            let addresses = (name, port).to_socket_addrs().map_err(|error| {
                Error::Network(format!("cannot resolve WebSocket host {name:?}: {error}"))
            })?;
            let mut any = false;
            for address in addresses {
                any = true;
                if !is_loopback(address.ip()) {
                    return Ok(false);
                }
            }
            Ok(any)
        }
        None => Err(Error::Config("WebSocket URL has no host".into())),
    }
}

fn is_loopback(address: IpAddr) -> bool {
    address.is_loopback()
}

/// Creates an opaque keepalive nonce.
pub fn keepalive_nonce() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[test]
    fn plaintext_is_limited_to_loopback() {
        assert!(
            validate_connect_url(&Url::parse("ws://127.0.0.1:8787/v1/tunnel").unwrap()).is_ok()
        );
        assert!(validate_connect_url(&Url::parse("ws://[::1]:8787/v1/tunnel").unwrap()).is_ok());
        assert!(validate_connect_url(&Url::parse("ws://example.com/v1/tunnel").unwrap()).is_err());
        assert!(validate_connect_url(&Url::parse("wss://example.com/v1/tunnel").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn reconnect_replays_unacknowledged_client_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first = accept_async(stream).await.unwrap();
            let open = first.next().await.unwrap().unwrap();
            assert!(matches!(open, Message::Text(_)));
            first
                .send(Message::Text(
                    Envelope::Ready {
                        tunnel_version: TUNNEL_VERSION,
                        connection_id: "connection".into(),
                        resume_token: Some("resume-secret".into()),
                        resumed: false,
                    }
                    .to_text()
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let first_frame = first.next().await.unwrap().unwrap();
            let Message::Text(first_frame) = first_frame else {
                panic!("expected first ACP frame");
            };
            assert!(matches!(
                Envelope::from_text(&first_frame).unwrap(),
                Envelope::Acp {
                    sequence: Some(1),
                    ..
                }
            ));
            drop(first);

            let (stream, _) = listener.accept().await.unwrap();
            let mut resumed = accept_async(stream).await.unwrap();
            let open = resumed.next().await.unwrap().unwrap();
            let Message::Text(open) = open else {
                panic!("expected resume open");
            };
            let Envelope::Open {
                resume: Some(resume),
                ..
            } = Envelope::from_text(&open).unwrap()
            else {
                panic!("expected resume credentials");
            };
            assert_eq!(resume.connection_id, "connection");
            assert_eq!(resume.resume_token, "resume-secret");
            resumed
                .send(Message::Text(
                    Envelope::Ready {
                        tunnel_version: TUNNEL_VERSION,
                        connection_id: "connection".into(),
                        resume_token: Some("resume-secret".into()),
                        resumed: true,
                    }
                    .to_text()
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let replay = resumed.next().await.unwrap().unwrap();
            let Message::Text(replay) = replay else {
                panic!("expected replayed ACP frame");
            };
            assert!(matches!(
                Envelope::from_text(&replay).unwrap(),
                Envelope::Acp {
                    sequence: Some(1),
                    payload
                } if payload == r#"{"id":1}"#
            ));
        });

        let options = ConnectOptions {
            url: Url::parse(&format!("ws://{address}/v1/tunnel")).unwrap(),
            agent: "fake".into(),
            workspace: "project".into(),
            token: "token".into(),
            max_frame_bytes: 1024,
            max_replay_frames: 8,
            max_replay_bytes: 8192,
            connection_timeout: Duration::from_secs(2),
            keepalive_timeout: Duration::from_secs(2),
            reconnect_timeout: Duration::from_secs(2),
        };
        let (mut socket, credentials) = open_socket(&options, None).await.unwrap();
        let pending = VecDeque::from([PendingFrame {
            sequence: 1,
            payload: r#"{"id":1}"#.into(),
        }]);
        send_acp(&mut socket, pending.front().unwrap())
            .await
            .unwrap();
        drop(socket);
        let resumed = reconnect_and_replay(&options, &credentials, &pending)
            .await
            .unwrap();
        drop(resumed);
        server.await.unwrap();
    }
}
