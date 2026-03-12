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
    /// Always `true` - we always spawn and manage the server process.
    pub managed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test that BusEvent parses correctly from SSE event JSON
    #[test]
    fn test_bus_event_parsing() {
        let json = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": "sess_123",
                "messageID": "msg_456",
                "field": "text",
                "delta": "Hello"
            }
        });

        let event: BusEvent = serde_json::from_value(json).expect("Failed to parse BusEvent");
        assert_eq!(event.event_type, "message.part.delta");
        assert!(event.properties.is_object());
    }

    /// Test that BusEvent handles missing properties with default
    #[test]
    fn test_bus_event_default_properties() {
        let json = json!({
            "type": "session.status"
        });

        let event: BusEvent = serde_json::from_value(json).expect("Failed to parse BusEvent");
        assert_eq!(event.event_type, "session.status");
        assert!(event.properties.is_null());
    }

    /// Test PartDeltaProperties parsing from valid JSON
    /// This verifies the Ok branch of the match statement in agent_overlay.rs
    #[test]
    fn test_part_delta_properties_valid() {
        let json = json!({
            "sessionID": "sess_abc123",
            "messageID": "msg_xyz789",
            "field": "text",
            "delta": "Hello, world!"
        });

        let props: PartDeltaProperties =
            serde_json::from_value(json).expect("Failed to parse PartDeltaProperties");
        assert_eq!(props.session_id, "sess_abc123");
        assert_eq!(props.message_id, "msg_xyz789");
        assert_eq!(props.field, "text");
        assert_eq!(props.delta, "Hello, world!");
    }

    /// Test PartDeltaProperties parsing from invalid JSON (missing required fields)
    /// This verifies the Err branch of the match statement in agent_overlay.rs
    #[test]
    fn test_part_delta_properties_invalid() {
        let json = json!({
            "sessionID": "sess_abc123"
            // Missing: messageID, field, delta
        });

        let result: Result<PartDeltaProperties, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "Expected parsing to fail for missing fields"
        );
    }

    /// Test PartDeltaProperties parsing from completely wrong type
    #[test]
    fn test_part_delta_properties_wrong_type() {
        let json = json!({
            "sessionID": 12345, // Wrong type (number instead of string)
            "messageID": "msg_xyz",
            "field": "text",
            "delta": "test"
        });

        let result: Result<PartDeltaProperties, _> = serde_json::from_value(json);
        assert!(result.is_err(), "Expected parsing to fail for wrong type");
    }

    /// Test PermissionRequest parsing from valid JSON
    /// This verifies the Ok branch of the match statement in agent_overlay.rs
    #[test]
    fn test_permission_request_valid() {
        let json = json!({
            "id": "per_123",
            "sessionID": "sess_abc",
            "permission": "edit",
            "patterns": ["src/main.rs", "src/lib.rs"],
            "metadata": {
                "filepath": "src/main.rs",
                "diff": "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,2 @@\n-old\n+new"
            }
        });

        let perm: PermissionRequest =
            serde_json::from_value(json).expect("Failed to parse PermissionRequest");
        assert_eq!(perm.id, "per_123");
        assert_eq!(perm.session_id, "sess_abc");
        assert_eq!(perm.permission, "edit");
        assert_eq!(perm.patterns, vec!["src/main.rs", "src/lib.rs"]);
        assert_eq!(perm.file_path(), Some("src/main.rs".to_string()));
        assert!(perm.diff().is_some());
    }

    /// Test PermissionRequest parsing from minimal valid JSON
    #[test]
    fn test_permission_request_minimal() {
        let json = json!({
            "id": "per_minimal",
            "sessionID": "sess_min",
            "permission": "bash"
        });

        let perm: PermissionRequest =
            serde_json::from_value(json).expect("Failed to parse PermissionRequest");
        assert_eq!(perm.id, "per_minimal");
        assert_eq!(perm.session_id, "sess_min");
        assert_eq!(perm.permission, "bash");
        assert!(perm.patterns.is_empty());
        assert!(perm.file_path().is_none());
    }

    /// Test PermissionRequest parsing from invalid JSON (missing required fields)
    /// This verifies the Err branch of the match statement in agent_overlay.rs
    #[test]
    fn test_permission_request_invalid() {
        let json = json!({
            "id": "per_123"
            // Missing: sessionID, permission
        });

        let result: Result<PermissionRequest, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "Expected parsing to fail for missing fields"
        );
    }

    /// Test PermissionRequest display_name method
    #[test]
    fn test_permission_request_display_name() {
        // With patterns
        let json = json!({
            "id": "per_1",
            "sessionID": "sess_1",
            "permission": "edit",
            "patterns": ["src/file.rs"]
        });
        let perm: PermissionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(perm.display_name(), "src/file.rs");

        // With file_path in metadata but no patterns
        let json = json!({
            "id": "per_2",
            "sessionID": "sess_2",
            "permission": "edit",
            "patterns": [],
            "metadata": {"filepath": "src/other.rs"}
        });
        let perm: PermissionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(perm.display_name(), "src/other.rs");

        // Fallback to permission name
        let json = json!({
            "id": "per_3",
            "sessionID": "sess_3",
            "permission": "bash",
            "patterns": []
        });
        let perm: PermissionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(perm.display_name(), "bash");
    }

    /// Test that the match statement pattern used in agent_overlay.rs works correctly
    /// This simulates the exact pattern: match serde_json::from_value::<PartDeltaProperties>(event.properties)
    #[test]
    fn test_match_pattern_simulation() {
        // Simulate Ok case
        let ok_json = json!({
            "sessionID": "sess_match",
            "messageID": "msg_match",
            "field": "text",
            "delta": "match test"
        });
        match serde_json::from_value::<PartDeltaProperties>(ok_json) {
            Ok(props) => {
                assert_eq!(props.session_id, "sess_match");
                assert_eq!(props.field, "text");
            }
            Err(e) => panic!("Expected Ok, got Err: {}", e),
        }

        // Simulate Err case
        let err_json = json!({"invalid": "data"});
        match serde_json::from_value::<PartDeltaProperties>(err_json) {
            Ok(_) => panic!("Expected Err, got Ok"),
            Err(e) => {
                // This is the expected path - verify error message exists
                assert!(!e.to_string().is_empty());
            }
        }
    }

    /// Test session.status event parsing (used in agent_overlay.rs)
    #[test]
    fn test_session_status_event() {
        let json = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "sess_status",
                "status": {
                    "type": "idle"
                }
            }
        });

        let event: BusEvent = serde_json::from_value(json).expect("Failed to parse BusEvent");
        assert_eq!(event.event_type, "session.status");

        // Verify the pattern used in agent_overlay.rs for session.status
        if let Some(props) = event.properties.as_object() {
            let session_id = props
                .get("sessionID")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            assert_eq!(session_id, "sess_status");

            let is_idle = props
                .get("status")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
                .map(|s| s == "idle")
                .unwrap_or(false);
            assert!(is_idle);
        } else {
            panic!("Expected properties to be an object");
        }
    }
}
