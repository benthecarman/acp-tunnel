use thiserror::Error;

/// Errors returned by acp-tunnel library APIs.
#[derive(Debug, Error)]
pub enum Error {
    /// A configuration value is invalid.
    #[error("configuration error: {0}")]
    Config(String),
    /// Authentication failed.
    #[error("authentication failed")]
    Unauthorized,
    /// A tunnel protocol message is invalid.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// An ACP policy rejected a request.
    #[error("policy error: {0}")]
    Policy(String),
    /// A child process operation failed.
    #[error("process error: {0}")]
    Process(String),
    /// A network operation failed.
    #[error("network error: {0}")]
    Network(String),
    /// An I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON serialization or parsing failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// TOML parsing failed.
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    /// A URL is invalid.
    #[error(transparent)]
    Url(#[from] url::ParseError),
    /// A WebSocket operation failed.
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    /// An operation exceeded its configured timeout.
    #[error("operation timed out: {0}")]
    Timeout(&'static str),
}

/// The result type used by acp-tunnel.
pub type Result<T> = std::result::Result<T, Error>;
