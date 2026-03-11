use crate::compositor::{Callback, Component, Compositor, Context, Event, EventResult};
use crate::job;
use crate::ui::overlay::Overlay;
use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::Editor;

use tui::buffer::Buffer as Surface;
use tui::text::Span;
use tui::widgets::{Block, Widget};

// =============================================================================
// Chat Message
// =============================================================================

/// A single message in the AI chat conversation.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Role of the sender: `"user"` or `"assistant"`.
    pub role: String,
    /// Message content (may span multiple lines).
    pub content: String,
}

// =============================================================================
// AgentOverlay
// =============================================================================

/// Full-screen overlay for AI agent interaction.
///
/// Layout:
/// ```text
/// ┌─ AI Agent ──────────────────────┐
/// │ You:                            │
/// │   What does this function do?   │
/// │                                 │
/// │ Assistant:                      │
/// │   It calculates the checksum…   │
/// │                                 │
/// │─────────────────────────────────│
/// │ > type here…                    │
/// └─────────────────────────────────┘
/// ```
pub struct AgentOverlay {
    /// Conversation history.
    messages: Vec<ChatMessage>,
    /// Current text in the input line.
    input: String,
    /// Byte-offset cursor position within `input`.
    input_cursor: usize,
    /// Scroll offset (in logical lines) for the output area.
    scroll_offset: usize,
    /// Whether we are waiting for a server response.
    loading: bool,
    /// Session ID assigned by the opencode server (None until first message).
    session_id: Option<String>,
    /// Optional agent ID to target when sending messages.
    agent_id: Option<String>,
    /// Display name for the agent shown in the overlay title.
    agent_name: String,
}

impl AgentOverlay {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            scroll_offset: 0,
            loading: false,
            session_id: None,
            agent_id: None,
            agent_name: "AI Agent".to_string(),
        }
    }

    /// Configure the overlay to target a specific agent.
    pub fn with_agent(mut self, id: String, name: String) -> Self {
        self.agent_id = Some(id);
        self.agent_name = name;
        self
    }

    /// Append a complete message to the conversation.
    pub fn push_message(&mut self, role: &str, content: &str) {
        self.messages.push(ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        });
    }

    /// Append text to the last assistant message, or create one if needed.
    pub fn append_to_last(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == "assistant" {
                last.content.push_str(text);
                return;
            }
        }
        self.push_message("assistant", text);
    }

    /// Set the loading state (waiting for AI response).
    #[allow(dead_code)]
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    // -------------------------------------------------------------------------
    // Rendering helpers
    // -------------------------------------------------------------------------

    /// Count the total logical lines produced by all messages when rendered.
    fn total_output_lines(&self, _width: usize) -> usize {
        let mut count = 0usize;
        for msg in &self.messages {
            // Role header line
            count += 1;
            // Content lines (one per line, no wrapping — set_stringn truncates)
            if msg.content.is_empty() {
                count += 1;
            } else {
                count += msg.content.lines().count();
            }
            // Blank separator between messages
            count += 1;
        }
        count
    }

    /// Render the message history into the output area.
    ///
    /// Styles are passed in to avoid borrow-checker conflicts with `Context`
    /// in the caller.
    fn render_messages(
        &self,
        area: Rect,
        surface: &mut Surface,
        user_style: helix_view::graphics::Style,
        assistant_style: helix_view::graphics::Style,
        role_label_style: helix_view::graphics::Style,
    ) {
        let max_y = area.y + area.height;
        let content_width = area.width.saturating_sub(2) as usize;
        let mut y = area.y;
        let mut line_idx: usize = 0;

        for msg in &self.messages {
            // -- Role header --------------------------------------------------
            if line_idx >= self.scroll_offset && y < max_y {
                let label = if msg.role == "user" {
                    "You:"
                } else {
                    "Assistant:"
                };
                let label_style = if msg.role == "user" {
                    user_style
                } else {
                    role_label_style
                };
                surface.set_stringn(area.x, y, label, area.width as usize, label_style);
                y += 1;
            }
            line_idx += 1;

            // -- Content lines ------------------------------------------------
            let style = if msg.role == "user" {
                user_style
            } else {
                assistant_style
            };

            if msg.content.is_empty() {
                // Empty content still occupies one logical line
                if line_idx >= self.scroll_offset && y < max_y {
                    y += 1;
                }
                line_idx += 1;
            } else {
                for line in msg.content.lines() {
                    // Render each line and let set_stringn handle display-width
                    // truncation safely (avoids UTF-8 boundary panics from
                    // manual byte-offset slicing).
                    if line_idx >= self.scroll_offset && y < max_y {
                        surface.set_stringn(
                            area.x + 2,
                            y,
                            line,
                            content_width,
                            style,
                        );
                        y += 1;
                    }
                    line_idx += 1;
                }
            }

            // -- Blank separator ----------------------------------------------
            if line_idx >= self.scroll_offset && y < max_y {
                y += 1;
            }
            line_idx += 1;
        }

        // Loading indicator
        if self.loading && y < max_y {
            surface.set_stringn(
                area.x + 2,
                y,
                "Thinking...",
                content_width,
                role_label_style,
            );
        }
    }
}

// =============================================================================
// Component implementation
// =============================================================================

/// Height reserved for the input area (border + prompt line + padding).
const INPUT_AREA_HEIGHT: u16 = 3;

impl Component for AgentOverlay {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        if area.width < 20 || area.height < 6 {
            return;
        }

        let theme = &cx.editor.theme;

        // -- Background -------------------------------------------------------
        let bg_style = theme.get("ui.background");
        surface.clear_with(area, bg_style);

        // -- Outer border -----------------------------------------------------
        let border_style = theme.get("ui.popup.info");
        let title = if self.loading {
            format!(" {} (loading…) ", self.agent_name)
        } else {
            format!(" {} ", self.agent_name)
        };
        let block = Block::bordered()
            .title(Span::styled(title, border_style))
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, surface);

        if inner.width < 4 || inner.height < INPUT_AREA_HEIGHT + 1 {
            return;
        }

        // -- Layout: output area (top) + input area (bottom) ------------------
        let output_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(INPUT_AREA_HEIGHT),
        };
        let input_area = Rect {
            x: inner.x,
            y: inner.y + output_area.height,
            width: inner.width,
            height: INPUT_AREA_HEIGHT,
        };

        // -- Extract styles before calling render_messages --------------------
        let user_style = theme.get("ui.text.focus");
        let assistant_style = theme.get("ui.text");
        let role_label_style = theme.get("ui.virtual.whitespace");
        let text_style = theme.get("ui.text");
        let prompt_style = theme.get("ui.text.focus");

        // -- Auto-scroll during streaming to keep latest content visible ------
        if self.loading {
            let total = self.total_output_lines(output_area.width as usize);
            let visible = output_area.height as usize;
            if total > visible {
                self.scroll_offset = total.saturating_sub(visible);
            }
        }

        // -- Render messages --------------------------------------------------
        self.render_messages(
            output_area,
            surface,
            user_style,
            assistant_style,
            role_label_style,
        );

        // -- Input separator line ---------------------------------------------
        let sep_y = input_area.y;
        for x in input_area.x..input_area.x + input_area.width {
            surface.set_stringn(x, sep_y, "─", 1, border_style);
        }

        // -- Input prompt -----------------------------------------------------
        let prompt_y = sep_y + 1;

        surface.set_stringn(input_area.x, prompt_y, "> ", 2, prompt_style);
        surface.set_stringn(
            input_area.x + 2,
            prompt_y,
            &self.input,
            input_area.width.saturating_sub(2) as usize,
            text_style,
        );
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match key.code {
            // -- Close overlay ------------------------------------------------
            KeyCode::Esc => {
                let close: Callback = Box::new(|compositor: &mut Compositor, _cx: &mut Context| {
                    compositor.pop();
                });
                EventResult::Consumed(Some(close))
            }

            // -- Submit message -----------------------------------------------
            KeyCode::Enter => {
                if !self.input.is_empty() && !self.loading {
                    let message = self.input.clone();
                    self.input.clear();
                    self.input_cursor = 0;
                    self.push_message("user", &message);
                    self.loading = true;

                    // Auto-scroll to show the user message
                    let total = self.total_output_lines(80);
                    self.scroll_offset = total.saturating_sub(10);

                    // Check if server is connected
                    let server = match &cx.editor.opencode_server {
                        Some(s) => s,
                        None => {
                            self.loading = false;
                            self.push_message(
                                "assistant",
                                "OpenCode server not connected. Run :ai-start first.",
                            );
                            return EventResult::Consumed(None);
                        }
                    };

                    let client = server.client().clone();
                    let session_id = self.session_id.clone();
                    let agent_id = self.agent_id.clone();

                    // Spawn async task that streams SSE deltas via
                    // job::dispatch(), falling back to synchronous send_message
                    // if the SSE connection fails.
                    cx.jobs.callback(async move {
                        // Create session if we don't have one yet
                        let sid = match session_id {
                            Some(id) => id,
                            None => match client.create_session().await {
                                Ok(session) => session.id,
                                Err(e) => {
                                    let err_msg = format!("Failed to create session: {}", e);
                                    job::dispatch(move |_editor, compositor| {
                                        if let Some(overlay) =
                                            compositor.find::<Overlay<AgentOverlay>>()
                                        {
                                            overlay.content.loading = false;
                                            overlay.content.push_message("assistant", &err_msg);
                                        }
                                    })
                                    .await;
                                    return Ok(job::Callback::EditorCompositor(Box::new(
                                        |_, _| {},
                                    )));
                                }
                            },
                        };

                        let session_id_for_events = sid.clone();

                        // Build the request, optionally targeting a specific agent
                        let build_request =
                            |msg: &str,
                             aid: &Option<String>|
                             -> helix_opencode::types::SendMessageRequest {
                                match aid {
                                    Some(id) => {
                                        helix_opencode::types::SendMessageRequest::text_with_agent(
                                            msg, id,
                                        )
                                    }
                                    None => {
                                        helix_opencode::types::SendMessageRequest::text(msg)
                                    }
                                }
                            };

                        // Try SSE streaming approach
                        match client.start_event_listener().await {
                            Ok(mut rx) => {
                                // SSE connected — send message asynchronously
                                let request = build_request(&message, &agent_id);
                                if let Err(e) = client.send_message_async(&sid, &request).await {
                                    let err_msg = format!("Error: {}", e);
                                    job::dispatch(move |_editor, compositor| {
                                        if let Some(overlay) =
                                            compositor.find::<Overlay<AgentOverlay>>()
                                        {
                                            overlay.content.loading = false;
                                            overlay.content.push_message("assistant", &err_msg);
                                        }
                                    })
                                    .await;
                                    return Ok(job::Callback::EditorCompositor(Box::new(
                                        |_, _| {},
                                    )));
                                }

                                // Process SSE events — each delta dispatches a
                                // UI update that triggers a re-render.
                                // Track whether the overlay is still alive so
                                // we can bail out of the loop early.
                                let cancelled = std::sync::Arc::new(
                                    std::sync::atomic::AtomicBool::new(false),
                                );
                                let session_id_clone = session_id_for_events.clone();

                                while let Some(event) = rx.recv().await {
                                    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                                        break;
                                    }
                                    match event.event_type.as_str() {
                                        "message.part.delta" => {
                                            if let Ok(props) = serde_json::from_value::<
                                                helix_opencode::types::PartDeltaProperties,
                                            >(
                                                event.properties
                                            ) {
                                                if props.session_id == session_id_clone
                                                    && props.field == "text"
                                                {
                                                    let delta = props.delta;
                                                    let flag = cancelled.clone();
                                                    job::dispatch(
                                                        move |_editor, compositor| {
                                                            if let Some(overlay) = compositor
                                                                .find::<Overlay<AgentOverlay>>()
                                                            {
                                                                overlay
                                                                    .content
                                                                    .append_to_last(&delta);
                                                            } else {
                                                                flag.store(
                                                                    true,
                                                                    std::sync::atomic::Ordering::Relaxed,
                                                                );
                                                            }
                                                        },
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        "session.status" => {
                                            // Check if our session went idle (response complete)
                                            if let Some(props) = event.properties.as_object() {
                                                let matches_session = props
                                                    .get("sessionID")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s == session_id_clone)
                                                    .unwrap_or(false);
                                                let is_idle = props
                                                    .get("status")
                                                    .and_then(|v| v.get("type"))
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s == "idle")
                                                    .unwrap_or(false);
                                                if matches_session && is_idle {
                                                    break;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }

                                // If no assistant message was ever created,
                                // push a placeholder so the user sees feedback.
                                if !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                                    let flag = cancelled.clone();
                                    job::dispatch(move |_editor, compositor| {
                                        if let Some(overlay) =
                                            compositor.find::<Overlay<AgentOverlay>>()
                                        {
                                            let has_assistant = overlay
                                                .content
                                                .messages
                                                .last()
                                                .map(|m| m.role == "assistant")
                                                .unwrap_or(false);
                                            if !has_assistant {
                                                overlay
                                                    .content
                                                    .push_message("assistant", "(No response)");
                                            }
                                        } else {
                                            flag.store(
                                                true,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                        }
                                    })
                                    .await;
                                }

                                // Done streaming — finalize session_id and loading state
                                let final_sid = session_id_for_events;
                                Ok(job::Callback::EditorCompositor(Box::new(
                                    move |_editor, compositor| {
                                        if let Some(overlay) =
                                            compositor.find::<Overlay<AgentOverlay>>()
                                        {
                                            overlay.content.session_id = Some(final_sid);
                                            overlay.content.loading = false;
                                        }
                                    },
                                )))
                            }
                            Err(_) => {
                                // SSE failed — fall back to synchronous send_message
                                let request = build_request(&message, &agent_id);
                                let response = client.send_message(&sid, &request).await;
                                let session_id_for_cb = sid;

                                Ok(job::Callback::EditorCompositor(Box::new(
                                    move |_editor, compositor| {
                                        let Some(overlay) =
                                            compositor.find::<Overlay<AgentOverlay>>()
                                        else {
                                            return;
                                        };
                                        let agent = &mut overlay.content;
                                        agent.loading = false;
                                        agent.session_id = Some(session_id_for_cb);
                                        match response {
                                            Ok(msg) => {
                                                let content = msg.text_content();
                                                if content.is_empty() {
                                                    agent.push_message(
                                                        "assistant",
                                                        "(No response)",
                                                    );
                                                } else {
                                                    agent.push_message("assistant", &content);
                                                }
                                            }
                                            Err(e) => {
                                                agent.push_message(
                                                    "assistant",
                                                    &format!("Error: {}", e),
                                                );
                                            }
                                        }
                                        // Auto-scroll to show the response
                                        let total_lines = agent.total_output_lines(80);
                                        agent.scroll_offset = total_lines.saturating_sub(20);
                                    },
                                )))
                            }
                        }
                    });
                }
                EventResult::Consumed(None)
            }

            // -- Text input ---------------------------------------------------
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.insert(self.input_cursor, c);
                self.input_cursor += c.len_utf8();
                EventResult::Consumed(None)
            }

            // -- Backspace ----------------------------------------------------
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    // Find the previous char boundary
                    let prev = self.input[..self.input_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(prev);
                    self.input_cursor = prev;
                }
                EventResult::Consumed(None)
            }

            // -- Cursor movement in input -------------------------------------
            KeyCode::Left => {
                if self.input_cursor > 0 {
                    self.input_cursor = self.input[..self.input_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                EventResult::Consumed(None)
            }
            KeyCode::Right => {
                if self.input_cursor < self.input.len() {
                    self.input_cursor = self.input[self.input_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input_cursor + i)
                        .unwrap_or(self.input.len());
                }
                EventResult::Consumed(None)
            }
            KeyCode::Home => {
                self.input_cursor = 0;
                EventResult::Consumed(None)
            }
            KeyCode::End => {
                self.input_cursor = self.input.len();
                EventResult::Consumed(None)
            }

            // -- Scroll output (PageUp / PageDown) ----------------------------
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                EventResult::Consumed(None)
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(20);
                EventResult::Consumed(None)
            }

            // -- Consume everything else so keys don't leak to editor ---------
            _ => EventResult::Consumed(None),
        }
    }

    fn cursor(&self, area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        // Place cursor in the input line.
        // inner area = area minus 1-cell border on each side
        let inner_y = area.y + area.height.saturating_sub(1 + INPUT_AREA_HEIGHT) + 1;
        let inner_x = area.x + 1; // border
        let prompt_len = 2u16; // "> "

        // Compute display column from byte cursor (ASCII-safe; for full
        // unicode we would need UnicodeWidthStr but this is fine for now).
        let display_col = self.input[..self.input_cursor].len() as u16;

        let cursor_x = inner_x + prompt_len + display_col;
        let cursor_y = inner_y;

        (
            Some(Position::new(cursor_y as usize, cursor_x as usize)),
            CursorKind::Block,
        )
    }

    fn required_size(&mut self, _viewport: (u16, u16)) -> Option<(u16, u16)> {
        // Full-screen — let the compositor decide the size.
        None
    }
}
