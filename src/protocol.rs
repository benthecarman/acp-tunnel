use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result};

/// The current tunnel protocol version.
pub const TUNNEL_VERSION: u32 = 3;

/// Metadata describing a connecting client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Client implementation name.
    pub name: String,
    /// Client implementation version.
    pub version: String,
}

/// Credentials used to resume one detached tunnel.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRequest {
    /// Opaque connection identifier returned by the original ready response.
    pub connection_id: String,
    /// Secret, single-session resume credential.
    pub resume_token: String,
}

impl fmt::Debug for ResumeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeRequest")
            .field("connection_id", &self.connection_id)
            .field("resume_token", &"[REDACTED]")
            .finish()
    }
}

/// Identifies the direction acknowledged by an [`Envelope::Ack`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStream {
    /// ACP sent from the connector to the remote agent.
    ClientToServer,
    /// ACP sent from the remote agent to the connector.
    ServerToClient,
}

/// Why a connector intentionally ends a remote-agent session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    /// The connector reached end-of-file on local standard input.
    StdinEof,
    /// The connector received SIGTERM.
    Sigterm,
    /// The connector received SIGINT or Ctrl-C.
    Interrupt,
    /// An embedding application requested shutdown.
    ClientShutdown,
    /// A reason introduced by a future tunnel version.
    Unknown(String),
}

impl ShutdownReason {
    fn as_str(&self) -> &str {
        match self {
            Self::StdinEof => "stdin_eof",
            Self::Sigterm => "sigterm",
            Self::Interrupt => "interrupt",
            Self::ClientShutdown => "client_shutdown",
            Self::Unknown(reason) => reason,
        }
    }
}

impl Serialize for ShutdownReason {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ShutdownReason {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let reason = String::deserialize(deserializer)?;
        Ok(match reason.as_str() {
            "stdin_eof" => Self::StdinEof,
            "sigterm" => Self::Sigterm,
            "interrupt" => Self::Interrupt,
            "client_shutdown" => Self::ClientShutdown,
            _ => Self::Unknown(reason),
        })
    }
}

/// Versioned messages exchanged over the tunnel WebSocket.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Envelope {
    /// Requests one configured agent and workspace.
    #[serde(rename_all = "camelCase")]
    Open {
        /// Tunnel protocol version.
        tunnel_version: u32,
        /// Server-configured agent identifier.
        agent: String,
        /// Server-configured workspace identifier.
        workspace: String,
        /// Connecting client metadata.
        client_info: ClientInfo,
        /// Resume credentials for a previously detached tunnel.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume: Option<ResumeRequest>,
    },
    /// Confirms that the remote agent is running.
    #[serde(rename_all = "camelCase")]
    Ready {
        /// Negotiated tunnel protocol version.
        tunnel_version: u32,
        /// Opaque connection identifier.
        connection_id: String,
        /// Secret required to reattach to this connection.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_token: Option<String>,
        /// True when this ready response confirms a resumed transport.
        #[serde(default)]
        resumed: bool,
    },
    /// Carries one complete, opaque ACP NDJSON line.
    Acp {
        /// Ordered stream sequence number in tunnel protocol v3.
        #[serde(skip_serializing_if = "Option::is_none")]
        sequence: Option<u64>,
        /// The original ACP line without its line terminator.
        payload: String,
    },
    /// Confirms durable delivery of sequenced ACP data to the next local pipe.
    Ack {
        /// Direction of the acknowledged ACP frame.
        stream: AckStream,
        /// Highest contiguous delivered sequence number.
        sequence: u64,
    },
    /// Carries one remote standard-error line.
    Stderr {
        /// Diagnostic text without its line terminator.
        payload: String,
    },
    /// Reports remote process termination.
    Exit {
        /// Portable process exit code, when available.
        code: Option<i32>,
        /// Unix signal number, when available.
        signal: Option<i32>,
    },
    /// Requests intentional termination of the remote-agent session.
    Shutdown {
        /// Stable connector shutdown reason.
        reason: ShutdownReason,
    },
    /// Confirms that intentional remote-agent shutdown is complete.
    #[serde(rename = "shutdown_complete")]
    ShutdownComplete {
        /// Portable process exit code, when available.
        code: Option<i32>,
        /// Unix signal number, when available.
        signal: Option<i32>,
    },
    /// Reports a tunnel-level error.
    Error {
        /// Stable machine-readable error category.
        code: String,
        /// Human-readable error safe to disclose to the client.
        message: String,
    },
    /// Tunnel-level keepalive request.
    Ping {
        /// Opaque value copied into the pong.
        nonce: String,
    },
    /// Tunnel-level keepalive response.
    Pong {
        /// Opaque value copied from the ping.
        nonce: String,
    },
}

impl fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open {
                tunnel_version,
                agent,
                workspace,
                client_info,
                resume,
            } => formatter
                .debug_struct("Open")
                .field("tunnel_version", tunnel_version)
                .field("agent", agent)
                .field("workspace", workspace)
                .field("client_info", client_info)
                .field("resume", resume)
                .finish(),
            Self::Ready {
                tunnel_version,
                connection_id,
                resume_token,
                resumed,
            } => formatter
                .debug_struct("Ready")
                .field("tunnel_version", tunnel_version)
                .field("connection_id", connection_id)
                .field("resume_token", &resume_token.as_ref().map(|_| "[REDACTED]"))
                .field("resumed", resumed)
                .finish(),
            Self::Acp { sequence, .. } => formatter
                .debug_struct("Acp")
                .field("sequence", sequence)
                .field("payload", &"[REDACTED]")
                .finish(),
            Self::Ack { stream, sequence } => formatter
                .debug_struct("Ack")
                .field("stream", stream)
                .field("sequence", sequence)
                .finish(),
            Self::Stderr { .. } => formatter
                .debug_struct("Stderr")
                .field("payload", &"[REDACTED]")
                .finish(),
            Self::Exit { code, signal } => formatter
                .debug_struct("Exit")
                .field("code", code)
                .field("signal", signal)
                .finish(),
            Self::Shutdown { reason } => formatter
                .debug_struct("Shutdown")
                .field("reason", reason)
                .finish(),
            Self::ShutdownComplete { code, signal } => formatter
                .debug_struct("ShutdownComplete")
                .field("code", code)
                .field("signal", signal)
                .finish(),
            Self::Error { code, message } => formatter
                .debug_struct("Error")
                .field("code", code)
                .field("message", message)
                .finish(),
            Self::Ping { nonce } => formatter
                .debug_struct("Ping")
                .field("nonce", nonce)
                .finish(),
            Self::Pong { nonce } => formatter
                .debug_struct("Pong")
                .field("nonce", nonce)
                .finish(),
        }
    }
}

impl Envelope {
    /// Serializes an envelope to a WebSocket text payload.
    pub fn to_text(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Parses one WebSocket text payload as a tunnel envelope.
    pub fn from_text(text: &str) -> Result<Self> {
        Ok(serde_json::from_str(text)?)
    }

    /// Validates and extracts an opening request.
    pub fn into_open(self) -> Result<OpenRequest> {
        match self {
            Self::Open {
                tunnel_version,
                agent,
                workspace,
                client_info,
                resume,
            } if tunnel_version == TUNNEL_VERSION => Ok(OpenRequest {
                tunnel_version,
                agent,
                workspace,
                client_info,
                resume,
            }),
            Self::Open { tunnel_version, .. } => Err(Error::Protocol(format!(
                "unsupported tunnel version {tunnel_version}; expected {TUNNEL_VERSION}"
            ))),
            _ => Err(Error::Protocol(
                "the first WebSocket message must be an open envelope".into(),
            )),
        }
    }
}

/// A validated opening request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenRequest {
    /// Requested tunnel protocol version.
    pub tunnel_version: u32,
    /// Requested configured agent identifier.
    pub agent: String,
    /// Requested configured workspace identifier.
    pub workspace: String,
    /// Connecting client metadata.
    pub client_info: ClientInfo,
    /// Resume credentials, when reconnecting a v3 tunnel.
    pub resume: Option<ResumeRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip_preserves_acp_exactly() {
        let payload = r#"{"jsonrpc":"2.0","method":"future/x","_meta":{"vendor":true}} "#;
        let envelope = Envelope::Acp {
            sequence: Some(7),
            payload: payload.into(),
        };
        let text = envelope.to_text().unwrap();
        assert_eq!(Envelope::from_text(&text).unwrap(), envelope);
        let Envelope::Acp {
            sequence,
            payload: result,
        } = Envelope::from_text(&text).unwrap()
        else {
            panic!("expected ACP envelope");
        };
        assert_eq!(sequence, Some(7));
        assert_eq!(result, payload);
    }

    #[test]
    fn rejects_unsupported_tunnel_version() {
        let open = Envelope::Open {
            tunnel_version: 99,
            agent: "codex".into(),
            workspace: "project-a".into(),
            client_info: ClientInfo {
                name: "test".into(),
                version: "1".into(),
            },
            resume: None,
        };
        assert!(matches!(open.into_open(), Err(Error::Protocol(_))));
    }

    #[test]
    fn rejects_tunnel_version_two() {
        let open = Envelope::Open {
            tunnel_version: 2,
            agent: "agent".into(),
            workspace: "workspace".into(),
            client_info: ClientInfo {
                name: "test".into(),
                version: "1".into(),
            },
            resume: None,
        };
        let error = open.into_open().unwrap_err().to_string();
        assert!(error.contains("unsupported tunnel version 2"));
        assert!(error.contains("expected 3"));
    }

    #[test]
    fn shutdown_envelopes_have_stable_names_and_accept_future_reasons() {
        let shutdown = Envelope::Shutdown {
            reason: ShutdownReason::StdinEof,
        };
        assert_eq!(
            serde_json::to_value(shutdown).unwrap(),
            serde_json::json!({"type":"shutdown","reason":"stdin_eof"})
        );
        let complete = Envelope::ShutdownComplete {
            code: Some(0),
            signal: None,
        };
        assert_eq!(
            serde_json::to_value(complete).unwrap(),
            serde_json::json!({"type":"shutdown_complete","code":0,"signal":null})
        );
        let future =
            Envelope::from_text(r#"{"type":"shutdown","reason":"future_reason"}"#).unwrap();
        assert!(matches!(
            future,
            Envelope::Shutdown {
                reason: ShutdownReason::Unknown(reason)
            } if reason == "future_reason"
        ));
    }

    #[test]
    fn debug_redacts_resume_credentials_and_payloads() {
        let open = Envelope::Open {
            tunnel_version: TUNNEL_VERSION,
            agent: "agent".into(),
            workspace: "workspace".into(),
            client_info: ClientInfo {
                name: "test".into(),
                version: "1".into(),
            },
            resume: Some(ResumeRequest {
                connection_id: "connection".into(),
                resume_token: "resume-debug-secret".into(),
            }),
        };
        let ready = Envelope::Ready {
            tunnel_version: TUNNEL_VERSION,
            connection_id: "connection".into(),
            resume_token: Some("ready-debug-secret".into()),
            resumed: true,
        };
        let acp = Envelope::Acp {
            sequence: Some(1),
            payload: "payload-debug-secret".into(),
        };
        let formatted = format!("{open:?} {ready:?} {acp:?}");
        for secret in [
            "resume-debug-secret",
            "ready-debug-secret",
            "payload-debug-secret",
        ] {
            assert!(!formatted.contains(secret));
        }
    }
}
