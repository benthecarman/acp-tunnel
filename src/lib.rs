#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
#![doc = "Reusable client, server, protocol, policy, and process APIs for acp-tunnel."]

/// Bearer authentication interfaces and implementations.
pub mod auth;
/// Local stdio tunnel client.
pub mod client;
/// Server-owned TOML configuration.
pub mod config;
/// Tunnel credential loading and secret representation.
pub mod credentials;
/// Typed library errors.
pub mod error;
/// Per-user default paths.
pub mod paths;
/// Narrow ACP workspace and MCP policy transformations.
pub mod policy;
/// Remote agent process lifecycle.
pub mod process;
/// Versioned WebSocket tunnel envelopes.
pub mod protocol;
/// Authenticated HTTP/WebSocket server.
pub mod server;
/// Server initialization, diagnostics, and service generation.
pub mod setup;

pub use error::{Error, Result};
