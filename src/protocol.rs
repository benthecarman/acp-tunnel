use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The current tunnel protocol version.
pub const TUNNEL_VERSION: u32 = 2;

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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRequest {
    /// Opaque connection identifier returned by the original ready response.
    pub connection_id: String,
    /// Secret, single-session resume credential.
    pub resume_token: String,
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

/// Versioned messages exchanged over the tunnel WebSocket.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
        /// Ordered stream sequence number in tunnel protocol v2.
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
    /// Resume credentials, when reconnecting a v2 tunnel.
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
}
