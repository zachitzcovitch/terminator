use std::fmt;

/// Errors that can occur when interacting with the OpenCode server.
#[derive(Debug)]
pub enum OpenCodeError {
    /// Server is not running or not reachable.
    ServerNotRunning,
    /// Failed to connect to the server.
    ConnectionFailed(String),
    /// Request timed out.
    Timeout,
    /// Server returned an invalid or unexpected response.
    InvalidResponse(String),
    /// Failed to spawn the server process.
    SpawnFailed(String),
    /// Server process crashed unexpectedly.
    ServerCrashed(String),
    /// HTTP error with status code.
    HttpError(u16, String),
    /// JSON serialization or deserialization error.
    JsonError(String),
}

impl fmt::Display for OpenCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerNotRunning => write!(f, "OpenCode server is not running"),
            Self::ConnectionFailed(msg) => write!(f, "Failed to connect to OpenCode: {msg}"),
            Self::Timeout => write!(f, "OpenCode request timed out"),
            Self::InvalidResponse(msg) => write!(f, "Invalid response from OpenCode: {msg}"),
            Self::SpawnFailed(msg) => write!(f, "Failed to start OpenCode server: {msg}"),
            Self::ServerCrashed(msg) => write!(f, "OpenCode server crashed: {msg}"),
            Self::HttpError(status, msg) => write!(f, "OpenCode HTTP error {status}: {msg}"),
            Self::JsonError(msg) => write!(f, "OpenCode JSON error: {msg}"),
        }
    }
}

impl std::error::Error for OpenCodeError {}

impl From<reqwest::Error> for OpenCodeError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::Timeout
        } else if err.is_connect() {
            Self::ConnectionFailed(err.to_string())
        } else {
            Self::ConnectionFailed(err.to_string())
        }
    }
}

impl From<serde_json::Error> for OpenCodeError {
    fn from(err: serde_json::Error) -> Self {
        Self::JsonError(err.to_string())
    }
}

/// Result type alias for OpenCode operations.
pub type Result<T> = std::result::Result<T, OpenCodeError>;
