// OpenCode HTTP client for communicating with the OpenCode server API.

use crate::error::{OpenCodeError, Result};
use crate::types::*;
use futures_util::StreamExt;
use reqwest::Client;
use std::time::Duration;
use tokio::sync::mpsc;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// HTTP client for communicating with the OpenCode server.
#[derive(Clone)]
pub struct OpenCodeClient {
    base_url: String,
    client: Client,
}

impl OpenCodeClient {
    /// Create a new client pointing at the given port on localhost.
    pub fn new(port: u16) -> Self {
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .expect("failed to build HTTP client");

        Self {
            base_url: format!("http://127.0.0.1:{}", port),
            client,
        }
    }

    /// Check if the server is healthy and reachable.
    pub async fn health(&self) -> Result<bool> {
        let url = format!("{}/global/health", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await?;

        Ok(resp.status().is_success())
    }

    /// List available agents.
    pub async fn list_agents(&self) -> Result<Vec<Agent>> {
        let url = format!("{}/agent", self.base_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(OpenCodeError::HttpError(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        let agents: Vec<Agent> = resp.json().await?;
        Ok(agents)
    }

    /// Create a new session.
    pub async fn create_session(&self) -> Result<Session> {
        let url = format!("{}/session", self.base_url);
        let resp = self.client.post(&url).send().await?;

        if !resp.status().is_success() {
            return Err(OpenCodeError::HttpError(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        let session: Session = resp.json().await?;
        Ok(session)
    }

    /// Send a message to a session and wait for the streamed response.
    ///
    /// The `/session/:id/message` endpoint streams the response body as
    /// chunked JSON. We read the full body, then parse the assistant message
    /// from the `{ info: {...}, parts: [...] }` envelope.
    pub async fn send_message(
        &self,
        session_id: &str,
        request: &SendMessageRequest,
    ) -> Result<Message> {
        let url = format!("{}/session/{}/message", self.base_url, session_id);
        let resp = self
            .client
            .post(&url)
            .json(request)
            .timeout(MESSAGE_TIMEOUT)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(OpenCodeError::HttpError(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        // Read the full streamed response body
        let body = resp
            .text()
            .await
            .map_err(|e| OpenCodeError::InvalidResponse(e.to_string()))?;

        // Parse the envelope: { "info": { "id": "...", ... }, "parts": [...] }
        let response: serde_json::Value = serde_json::from_str(&body)?;

        let parts = response
            .get("parts")
            .and_then(|p| serde_json::from_value::<Vec<MessagePart>>(p.clone()).ok())
            .unwrap_or_default();

        let id = response
            .get("info")
            .and_then(|i| i.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or("")
            .to_string();

        Ok(Message {
            id,
            role: "assistant".to_string(),
            parts,
            session_id: Some(session_id.to_string()),
        })
    }

    /// Get all messages in a session.
    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let url = format!("{}/session/{}/message", self.base_url, session_id);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(OpenCodeError::HttpError(
                resp.status().as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
        }

        let messages: Vec<Message> = resp.json().await?;
        Ok(messages)
    }

    /// Send a message asynchronously (fire-and-forget).
    ///
    /// Uses the `/session/:id/prompt_async` endpoint which returns 204
    /// immediately. The actual response arrives via SSE events.
    pub async fn send_message_async(
        &self,
        session_id: &str,
        request: &SendMessageRequest,
    ) -> Result<()> {
        let url = format!("{}/session/{}/prompt_async", self.base_url, session_id);
        let resp = self
            .client
            .post(&url)
            .json(request)
            .timeout(DEFAULT_TIMEOUT)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(OpenCodeError::HttpError(status, body));
        }

        Ok(())
    }

    /// Connect to the SSE `/event` stream and return a channel receiver.
    ///
    /// Spawns a background task that reads the SSE stream, parses events,
    /// and forwards them through the channel. The task exits when the
    /// receiver is dropped or the connection breaks.
    pub async fn start_event_listener(&self) -> Result<mpsc::UnboundedReceiver<BusEvent>> {
        let url = format!("{}/event", self.base_url);

        // Build a separate client without the default timeout — SSE
        // connections are long-lived and must not time out.
        let sse_client = Client::builder()
            .build()
            .map_err(|e| OpenCodeError::ConnectionFailed(e.to_string()))?;

        let resp = sse_client
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(OpenCodeError::HttpError(
                resp.status().as_u16(),
                "SSE connection failed".into(),
            ));
        }

        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        log::error!("SSE stream error: {}", e);
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&bytes));

                // SSE events are separated by blank lines (\n\n).
                // Process all complete events in the buffer.
                while let Some(pos) = buffer.find("\n\n") {
                    let event_text = buffer[..pos].to_string();
                    buffer = buffer[pos + 2..].to_string();

                    // Each SSE event may have multiple lines; we only
                    // care about "data: {...}" lines.
                    for line in event_text.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            match serde_json::from_str::<BusEvent>(data) {
                                Ok(event) => {
                                    if tx.send(event).is_err() {
                                        // Receiver dropped — stop the task.
                                        return;
                                    }
                                }
                                Err(e) => {
                                    log::warn!("SSE parse skip: {}", e);
                                }
                            }
                        }
                    }
                }
            }

            log::debug!("SSE event listener ended");
        });

        Ok(rx)
    }

    /// Reply to a permission request.
    ///
    /// `reply` must be one of:
    /// - `"once"` — approve this time only
    /// - `"always"` — auto-approve future identical requests
    /// - `"reject"` — deny the request
    ///
    /// `message` is optional feedback the agent will see (useful when rejecting).
    pub async fn reply_permission(
        &self,
        request_id: &str,
        reply: &str,
        message: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/permission/{}/reply", self.base_url, request_id);

        let mut body = serde_json::json!({ "reply": reply });
        if let Some(msg) = message {
            body["message"] = serde_json::Value::String(msg.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(OpenCodeError::HttpError(status, body));
        }

        Ok(())
    }

    /// Get the base URL this client is configured to use.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}
