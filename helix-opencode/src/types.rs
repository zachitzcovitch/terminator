// API request/response types for the OpenCode server protocol.

use serde::{Deserialize, Serialize};

/// Response from GET /global/health
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
}

/// An OpenCode session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "agentID", default)]
    pub agent_id: Option<String>,
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<String>,
}

/// An available agent.
#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

/// A message part (text content).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessagePart {
    #[serde(rename = "text")]
    Text { text: String },
    /// Catch-all for unknown part types returned by the server.
    #[serde(other)]
    Unknown,
}

/// A message in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    /// One of "user", "assistant", or "tool".
    pub role: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
    #[serde(rename = "sessionID", default)]
    pub session_id: Option<String>,
}

impl Message {
    /// Get the concatenated text content of this message.
    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text } => Some(text.as_str()),
                MessagePart::Unknown => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Request body for POST /session/:id/message.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    pub parts: Vec<MessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl SendMessageRequest {
    /// Create a simple text message.
    pub fn text(content: &str) -> Self {
        Self {
            parts: vec![MessagePart::Text {
                text: content.to_string(),
            }],
            agent: None,
        }
    }

    /// Create a text message targeted at a specific agent.
    pub fn text_with_agent(content: &str, agent_id: &str) -> Self {
        Self {
            parts: vec![MessagePart::Text {
                text: content.to_string(),
            }],
            agent: Some(agent_id.to_string()),
        }
    }
}

/// SSE event from the /event stream.
#[derive(Debug, Clone, Deserialize)]
pub struct BusEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub properties: serde_json::Value,
}

/// Properties for `message.part.delta` events (streaming text chunks).
#[derive(Debug, Clone, Deserialize)]
pub struct PartDeltaProperties {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub field: String,
    pub delta: String,
}

/// A permission request from the OpenCode server.
/// Emitted as SSE event "permission.asked" when a tool needs approval.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequest {
    /// Unique permission request ID (e.g., "per_...")
    pub id: String,
    /// Session this permission belongs to
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Permission type (e.g., "edit", "bash", "external_directory")
    pub permission: String,
    /// Patterns being acted on (e.g., relative file paths)
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Tool-specific metadata (filepath, diff, files array for apply_patch)
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Tool call info
    #[serde(default)]
    pub tool: Option<serde_json::Value>,
}

impl PermissionRequest {
    /// Get the file path from metadata (works for edit, write, apply_patch).
    pub fn file_path(&self) -> Option<String> {
        self.metadata
            .get("filepath")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Get the unified diff from metadata.
    pub fn diff(&self) -> Option<String> {
        self.metadata
            .get("diff")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Get the display name (first pattern or file path).
    pub fn display_name(&self) -> String {
        self.patterns
            .first()
            .cloned()
            .or_else(|| self.file_path())
            .unwrap_or_else(|| self.permission.clone())
    }
}

/// Internal bookkeeping for a running OpenCode server.
/// Not serialized — used only within the client.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub port: u16,
    pub url: String,
    /// `true` if we spawned the process, `false` if it was already running.
    pub managed: bool,
}
