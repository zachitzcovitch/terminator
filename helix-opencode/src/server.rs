// OpenCode server lifecycle management (spawn, health check, shutdown).

use crate::client::OpenCodeClient;
use crate::error::{OpenCodeError, Result};
use crate::types::ServerInfo;
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration};

/// How often to poll the server during startup health checks.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum time to wait for the server to become healthy after spawning.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Manages the OpenCode server child process lifecycle.
///
/// If we spawned the process ourselves (`managed = true`), we own it and
/// will kill it on shutdown or drop. If we connected to a pre-existing
/// server, we leave it alone.
pub struct OpenCodeServer {
    /// Child process handle. `None` when reusing an external server.
    child: Option<Child>,
    /// HTTP client for communicating with the server.
    client: OpenCodeClient,
    /// Metadata about the running server.
    info: ServerInfo,
}

impl OpenCodeServer {
    /// Start or connect to an OpenCode server.
    ///
    /// First checks whether a server is already listening on `port`.
    /// If so, reuses it without spawning. Otherwise, runs
    /// `<opencode_path> serve --port <port>` and waits for it to
    /// become healthy (up to 30 seconds).
    pub async fn start(port: u16, opencode_path: &str) -> Result<Self> {
        let client = OpenCodeClient::new(port);

        // Reuse an already-running server if one responds on this port.
        if client.health().await.unwrap_or(false) {
            log::info!("OpenCode server already running on port {}", port);
            return Ok(Self {
                child: None,
                client,
                info: ServerInfo {
                    port,
                    url: format!("http://127.0.0.1:{}", port),
                    managed: false,
                },
            });
        }

        // Spawn a new server process.
        log::info!("Spawning OpenCode server on port {}...", port);
        let child = Command::new(opencode_path)
            .arg("serve")
            .arg("--port")
            .arg(port.to_string())
            .env("OPENCODE_PERMISSION", r#"{"edit":"ask"}"#)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                OpenCodeError::SpawnFailed(format!(
                    "Failed to run '{}': {}. Is opencode installed?",
                    opencode_path, e
                ))
            })?;

        // Poll until the server responds to health checks.
        let max_attempts =
            (STARTUP_TIMEOUT.as_millis() / HEALTH_POLL_INTERVAL.as_millis()) as u32;
        for attempt in 1..=max_attempts {
            sleep(HEALTH_POLL_INTERVAL).await;

            if client.health().await.unwrap_or(false) {
                log::info!(
                    "OpenCode server ready on port {} (after {}ms)",
                    port,
                    attempt as u64 * HEALTH_POLL_INTERVAL.as_millis() as u64
                );
                return Ok(Self {
                    child: Some(child),
                    client,
                    info: ServerInfo {
                        port,
                        url: format!("http://127.0.0.1:{}", port),
                        managed: true,
                    },
                });
            }
        }

        Err(OpenCodeError::Timeout)
    }

    /// Returns `true` if the server responds to a health check.
    pub async fn is_running(&self) -> bool {
        self.client.health().await.unwrap_or(false)
    }

    /// Borrow the HTTP client for making API calls.
    pub fn client(&self) -> &OpenCodeClient {
        &self.client
    }

    /// Metadata about the running server (port, url, managed flag).
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// Shut down the server gracefully.
    ///
    /// Only kills the process if we spawned it (`managed = true`).
    /// Reused external servers are left untouched.
    pub async fn shutdown(&mut self) {
        if let Some(ref mut child) = self.child {
            log::info!("Shutting down managed OpenCode server...");
            let _ = child.kill().await;
            log::info!("OpenCode server stopped");
        }
        self.child = None;
    }
}

impl Drop for OpenCodeServer {
    fn drop(&mut self) {
        // `kill_on_drop(true)` on the Child already handles cleanup, but
        // we call `start_kill()` explicitly as a safety net so the signal
        // is sent immediately rather than waiting for the tokio runtime to
        // reap the handle.
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
    }
}
