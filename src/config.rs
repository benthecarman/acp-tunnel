use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use regex::Regex;
use serde::Deserialize;

use crate::{Error, Result};

const DEFAULT_MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;

/// Complete server-owned configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Listener address. A CLI value may override it.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Maximum ACP line and WebSocket message size.
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,
    /// Maximum time for connection setup and the opening message.
    #[serde(default = "default_connection_timeout_seconds")]
    pub connection_timeout_seconds: u64,
    /// Interval between tunnel keepalive messages.
    #[serde(default = "default_keepalive_interval_seconds")]
    pub keepalive_interval_seconds: u64,
    /// Maximum time without a valid tunnel message.
    #[serde(default = "default_keepalive_timeout_seconds")]
    pub keepalive_timeout_seconds: u64,
    /// Maximum time allowed for graceful process shutdown.
    #[serde(default = "default_shutdown_timeout_seconds")]
    pub shutdown_timeout_seconds: u64,
    /// Time a remote process remains alive while its client reconnects.
    #[serde(default = "default_reconnect_grace_seconds")]
    pub reconnect_grace_seconds: u64,
    /// Maximum unacknowledged ACP frames retained for replay.
    #[serde(default = "default_max_replay_frames")]
    pub max_replay_frames: usize,
    /// Maximum unacknowledged ACP payload bytes retained for replay.
    #[serde(default = "default_max_replay_bytes")]
    pub max_replay_bytes: usize,
    /// Capacity of the outgoing ACP message channel.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
    /// Capacity of the best-effort remote stderr channel.
    #[serde(default = "default_diagnostic_channel_capacity")]
    pub diagnostic_channel_capacity: usize,
    /// Maximum size of one remote stderr line.
    #[serde(default = "default_diagnostic_line_bytes")]
    pub diagnostic_line_bytes: usize,
    /// Rewrites ACP lifecycle `params.cwd` to the selected remote workspace.
    #[serde(default = "default_true")]
    pub rewrite_cwd: bool,
    /// Explicit opt-in required when any agent uses MCP passthrough.
    #[serde(default)]
    pub allow_insecure_mcp_passthrough: bool,
    /// Exact browser Origin values allowed on WebSocket upgrades.
    #[serde(default)]
    pub allowed_origins: BTreeSet<String>,
    /// Optional direct TLS configuration.
    pub tls: Option<TlsConfig>,
    /// Allowlisted ACP agents, keyed by public identifier.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    /// Allowlisted remote workspaces, keyed by public identifier.
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceConfig>,
    /// Allowlisted server-owned MCP commands, keyed by incoming `name`.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

/// Direct TLS certificate and private-key paths.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain path.
    pub cert_path: PathBuf,
    /// PEM private key path.
    pub key_path: PathBuf,
}

/// One allowlisted ACP agent process definition.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Executable path or name.
    pub command: PathBuf,
    /// Fixed server-owned command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Workspace identifiers this agent may use.
    #[serde(default)]
    pub workspaces: BTreeSet<String>,
    /// Host environment variable names that may be inherited.
    #[serde(default)]
    pub pass_env: BTreeSet<String>,
    /// Fixed nonsecret environment values.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Policy applied to client-provided MCP servers.
    #[serde(default)]
    pub mcp_policy: McpPolicy,
}

/// One server-owned workspace.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Absolute path that must already exist on the server.
    pub path: PathBuf,
}

/// One server-owned stdio MCP server definition.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Executable path or name.
    pub command: PathBuf,
    /// Fixed server-owned command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Host environment variable names that may be copied into ACP configuration.
    #[serde(default)]
    pub pass_env: BTreeSet<String>,
    /// Fixed nonsecret environment values.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Policy for MCP server configuration inside `session/new`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpPolicy {
    /// Remove all client-provided MCP server definitions.
    Deny,
    /// Replace each named definition from the server allowlist.
    #[default]
    Allowlisted,
    /// Forward definitions unchanged. This permits remote code execution.
    Passthrough,
}

impl ServerConfig {
    /// Loads and validates a TOML configuration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|error| {
            Error::Config(format!("cannot read {}: {error}", path.as_ref().display()))
        })?;
        let config: Self = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates identifiers, limits, paths, references, and insecure options.
    pub fn validate(&self) -> Result<()> {
        if self.max_frame_bytes == 0
            || self.channel_capacity == 0
            || self.diagnostic_channel_capacity == 0
            || self.diagnostic_line_bytes == 0
            || self.max_replay_frames == 0
            || self.max_replay_bytes == 0
        {
            return Err(Error::Config(
                "frame and channel limits must be greater than zero".into(),
            ));
        }
        if self.max_replay_bytes < self.max_frame_bytes {
            return Err(Error::Config(
                "max_replay_bytes must be at least max_frame_bytes".into(),
            ));
        }
        if self.connection_timeout_seconds == 0
            || self.keepalive_interval_seconds == 0
            || self.keepalive_timeout_seconds <= self.keepalive_interval_seconds
            || self.shutdown_timeout_seconds == 0
            || self.reconnect_grace_seconds == 0
        {
            return Err(Error::Config(
                "timeouts must be positive and keepalive_timeout_seconds must exceed keepalive_interval_seconds"
                    .into(),
            ));
        }

        for (id, workspace) in &self.workspaces {
            validate_id("workspace", id)?;
            if !workspace.path.is_absolute() {
                return Err(Error::Config(format!(
                    "workspace {id:?} path must be absolute"
                )));
            }
        }

        for (id, agent) in &self.agents {
            validate_id("agent", id)?;
            validate_command("agent", id, &agent.command)?;
            for workspace in &agent.workspaces {
                validate_id("workspace", workspace)?;
                if !self.workspaces.contains_key(workspace) {
                    return Err(Error::Config(format!(
                        "agent {id:?} references unknown workspace {workspace:?}"
                    )));
                }
            }
            validate_environment("agent", id, &agent.pass_env, &agent.env)?;
            if agent.mcp_policy == McpPolicy::Passthrough && !self.allow_insecure_mcp_passthrough {
                return Err(Error::Config(format!(
                    "agent {id:?} uses MCP passthrough; set allow_insecure_mcp_passthrough = true to acknowledge remote command execution risk"
                )));
            }
        }

        for (id, server) in &self.mcp_servers {
            validate_id("MCP server", id)?;
            validate_command("MCP server", id, &server.command)?;
            validate_environment("MCP server", id, &server.pass_env, &server.env)?;
        }

        for origin in &self.allowed_origins {
            let parsed = url::Url::parse(origin).map_err(|error| {
                Error::Config(format!("invalid allowed origin {origin:?}: {error}"))
            })?;
            if parsed.cannot_be_a_base() && parsed.scheme() != "null" {
                return Err(Error::Config(format!(
                    "allowed origin {origin:?} must be an absolute origin"
                )));
            }
        }

        if let Some(tls) = &self.tls
            && (tls.cert_path == tls.key_path)
        {
            return Err(Error::Config(
                "TLS certificate and private key paths must differ".into(),
            ));
        }
        Ok(())
    }

    /// Connection setup timeout.
    pub fn connection_timeout(&self) -> Duration {
        Duration::from_secs(self.connection_timeout_seconds)
    }

    /// Tunnel keepalive interval.
    pub fn keepalive_interval(&self) -> Duration {
        Duration::from_secs(self.keepalive_interval_seconds)
    }

    /// Tunnel keepalive timeout.
    pub fn keepalive_timeout(&self) -> Duration {
        Duration::from_secs(self.keepalive_timeout_seconds)
    }

    /// Child shutdown timeout.
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.shutdown_timeout_seconds)
    }

    /// Maximum time a detached agent session waits for a valid resume.
    pub fn reconnect_grace(&self) -> Duration {
        Duration::from_secs(self.reconnect_grace_seconds)
    }
}

/// Validates a public agent, workspace, or MCP identifier.
pub fn validate_id(kind: &str, id: &str) -> Result<()> {
    let pattern = Regex::new(r"^[a-z0-9][a-z0-9_-]*$")
        .map_err(|error| Error::Config(format!("internal identifier pattern error: {error}")))?;
    if pattern.is_match(id) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "invalid {kind} identifier {id:?}; expected [a-z0-9][a-z0-9_-]*"
        )))
    }
}

fn validate_command(kind: &str, id: &str, command: &Path) -> Result<()> {
    if command.as_os_str().is_empty() {
        Err(Error::Config(format!(
            "{kind} {id:?} command must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn validate_environment(
    kind: &str,
    id: &str,
    pass_env: &BTreeSet<String>,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    for name in pass_env.iter().chain(env.keys()) {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(Error::Config(format!(
                "{kind} {id:?} has invalid environment variable name"
            )));
        }
    }
    let overlap: Vec<_> = pass_env
        .iter()
        .filter(|name| env.contains_key(*name))
        .collect();
    if overlap.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "{kind} {id:?} lists the same variable in pass_env and env"
        )))
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}
fn default_max_frame_bytes() -> usize {
    DEFAULT_MAX_FRAME_BYTES
}
fn default_connection_timeout_seconds() -> u64 {
    10
}
fn default_keepalive_interval_seconds() -> u64 {
    15
}
fn default_keepalive_timeout_seconds() -> u64 {
    45
}
fn default_shutdown_timeout_seconds() -> u64 {
    5
}
fn default_reconnect_grace_seconds() -> u64 {
    30
}
fn default_max_replay_frames() -> usize {
    256
}
fn default_max_replay_bytes() -> usize {
    20 * 1024 * 1024
}
fn default_channel_capacity() -> usize {
    32
}
fn default_diagnostic_channel_capacity() -> usize {
    64
}
fn default_diagnostic_line_bytes() -> usize {
    64 * 1024
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> std::result::Result<ServerConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn validates_identifiers() {
        for valid in ["a", "project-a", "codex_2", "0"] {
            assert!(validate_id("test", valid).is_ok());
        }
        for invalid in ["", "-a", "Upper", "a.b", "../x", "two words"] {
            assert!(validate_id("test", invalid).is_err());
        }
    }

    #[test]
    fn rejects_unknown_workspace_and_agent_identifier() {
        let config = parse(
            r#"
            [agents."../bad"]
            command = "agent"
            workspaces = ["missing"]
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn passthrough_needs_explicit_opt_in() {
        let config = parse(
            r#"
            [agents.test]
            command = "agent"
            mcp_policy = "passthrough"
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
