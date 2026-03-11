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
        }
    }

    /// Append a complete message to the conversation.
    pub fn push_message(&mut self, role: &str, content: &str) {
        self.messages.push(ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        });
    }

    /// Set the loading state (waiting for AI response).
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    /// Append text to the last assistant message (for streaming responses).
    ///
    /// If the last message is not from the assistant, a new assistant message
    /// is created automatically.
    pub fn append_to_last(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == "assistant" {
                last.content.push_str(text);
                return;
            }
        }
        self.push_message("assistant", text);
    }

    // -------------------------------------------------------------------------
    // Rendering helpers
    // -------------------------------------------------------------------------

    /// Count the total logical lines produced by all messages when rendered
    /// into the given `width`.
    fn total_output_lines(&self, width: usize) -> usize {
        let w = width.max(1);
        let mut count = 0usize;
        for msg in &self.messages {
            // Role header line
            count += 1;
            // Content lines (simple char-width wrapping estimate)
            if msg.content.is_empty() {
                count += 1;
            } else {
                for line in msg.content.lines() {
                    count += 1 + line.len() / w;
                }
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
                    // Simple wrapping: each physical line may span multiple rows
                    let wrapped_rows = 1 + line.len() / content_width.max(1);
                    for row in 0..wrapped_rows {
                        if line_idx >= self.scroll_offset && y < max_y {
                            let start = row * content_width;
                            let end = ((row + 1) * content_width).min(line.len());
                            if start < line.len() {
                                surface.set_stringn(
                                    area.x + 2,
                                    y,
                                    &line[start..end],
                                    content_width,
                                    style,
                                );
                            }
                            y += 1;
                        }
                        line_idx += 1;
                    }
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
            " AI Agent (loading…) "
        } else {
            " AI Agent "
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

                    // Send the message asynchronously via a job callback
                    cx.jobs.callback(async move {
                        // Create session if we don't have one yet
                        let sid = match session_id {
                            Some(id) => id,
                            None => match client.create_session().await {
                                Ok(session) => session.id,
                                Err(e) => {
                                    let err_msg = format!("Failed to create session: {}", e);
                                    let callback: job::Callback = job::Callback::EditorCompositor(
                                        Box::new(move |_editor, compositor| {
                                            if let Some(overlay) =
                                                compositor.find::<Overlay<AgentOverlay>>()
                                            {
                                                overlay.content.loading = false;
                                                overlay.content.push_message("assistant", &err_msg);
                                            }
                                        }),
                                    );
                                    return Ok(callback);
                                }
                            },
                        };

                        let session_id_for_callback = sid.clone();

                        // Send the message and get the assistant response directly
                        let request =
                            helix_opencode::types::SendMessageRequest::text(&message);
                        let response = client.send_message(&sid, &request).await;

                        let callback: job::Callback =
                            job::Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                                let Some(overlay) =
                                    compositor.find::<Overlay<AgentOverlay>>()
                                else {
                                    return;
                                };
                                let agent = &mut overlay.content;
                                agent.loading = false;
                                agent.session_id = Some(session_id_for_callback);

                                match response {
                                    Ok(msg) => {
                                        let content = msg.text_content();
                                        if !content.is_empty() {
                                            agent.push_message("assistant", &content);
                                        } else {
                                            agent.push_message("assistant", "(No response)");
                                        }
                                    }
                                    Err(e) => {
                                        agent.push_message(
                                            "assistant",
                                            &format!("Error: {}", e),
                                        );
                                    }
                                }

                                // Auto-scroll to bottom
                                let total_lines = agent.total_output_lines(80);
                                agent.scroll_offset = total_lines.saturating_sub(20);
                            }));
                        Ok(callback)
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
