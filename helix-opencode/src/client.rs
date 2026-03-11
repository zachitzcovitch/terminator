// OpenCode HTTP client for communicating with the OpenCode server API.

use crate::error::{OpenCodeError, Result};
use crate::types::*;
use reqwest::Client;
use std::time::Duration;

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

    /// Get the base URL this client is configured to use.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}
