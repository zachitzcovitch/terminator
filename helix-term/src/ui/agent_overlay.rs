use crate::compositor::{Callback, Component, Compositor, Context, Event, EventResult};
use crate::job;
use crate::ui::overlay::Overlay;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::Position;
use helix_view::graphics::{CursorKind, Modifier, Rect, Style};
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::Theme;
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
// Display Line Cache
// =============================================================================

/// A pre-computed line for rendering in the overlay.
#[derive(Debug, Clone)]
enum DisplayLine {
    /// Role header (e.g., "You:", "Assistant:")
    RoleHeader { text: String, style: Style },
    /// Content line with styled segments
    Content {
        segments: Vec<(String, Style)>,
        #[allow(dead_code)]
        is_code_block: bool,
        is_user: bool,
    },
    /// Blank line for spacing
    BlankLine,
}

/// Word-wrap a single line of text at `max_width` display columns.
///
/// Splits on whitespace boundaries and measures each word using
/// `UnicodeWidthStr` so that CJK and other wide characters are
/// handled correctly.
fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width: usize = 0;

    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);

        // If adding this word (plus a space separator) would exceed the
        // limit, flush the current line first.
        let separator_cost = if current_line.is_empty() { 0 } else { 1 };
        if current_width + separator_cost + word_width > max_width {
            if !current_line.is_empty() {
                lines.push(current_line);
                current_line = String::new();
                current_width = 0;
            }
            // If a single word is wider than max_width, push it as-is
            // (set_stringn will truncate on render).
            if word_width > max_width {
                lines.push(word.to_string());
                continue;
            }
        }

        if !current_line.is_empty() {
            current_line.push(' ');
            current_width += 1;
        }
        current_line.push_str(word);
        current_width += word_width;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    // An empty input should still produce one blank line.
    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

/// Parse a line of text for markdown-like inline formatting.
///
/// Recognises fenced code blocks (``` delimiters), ATX headers (`#`),
/// unordered list items (`-` / `*`), inline code (`` ` ``), and bold
/// (`**…**`).  Unclosed formatting markers are emitted as plain text.
fn parse_markdown_segments(
    line: &str,
    in_code_block: &mut bool,
    base_style: Style,
    code_style: Style,
    bold_style: Style,
    inline_code_style: Style,
) -> Vec<(String, Style)> {
    // Inside a fenced code block — everything is rendered with code_style.
    if *in_code_block {
        if line.trim_start().starts_with("```") {
            *in_code_block = false;
        }
        return vec![(line.to_string(), code_style)];
    }

    // Opening a fenced code block.
    if line.trim_start().starts_with("```") {
        *in_code_block = true;
        return vec![(line.to_string(), code_style)];
    }

    // ATX header — strip leading `#` markers and render bold.
    if line.starts_with("# ") || line.starts_with("## ") || line.starts_with("### ") {
        let text = line.trim_start_matches('#').trim_start();
        return vec![(text.to_string(), bold_style)];
    }

    // Unordered list items — replace marker with a bullet character.
    let (prefix, rest) = if line.starts_with("- ") || line.starts_with("* ") {
        ("  • ".to_string(), &line[2..])
    } else if line.starts_with("  - ") || line.starts_with("  * ") {
        ("    ◦ ".to_string(), &line[4..])
    } else {
        (String::new(), line)
    };

    // Parse inline formatting: **bold** and `code`.
    let mut segments: Vec<(String, Style)> = Vec::new();
    if !prefix.is_empty() {
        segments.push((prefix, base_style));
    }

    let mut chars = rest.chars().peekable();
    let mut current = String::new();

    while let Some(c) = chars.next() {
        match c {
            '`' => {
                // Flush accumulated plain text.
                if !current.is_empty() {
                    segments.push((current.clone(), base_style));
                    current.clear();
                }
                let mut code_text = String::new();
                let mut closed = false;
                while let Some(&next) = chars.peek() {
                    if next == '`' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    code_text.push(chars.next().unwrap());
                }
                if closed && !code_text.is_empty() {
                    segments.push((code_text, inline_code_style));
                } else {
                    // Unclosed backtick — emit as plain text.
                    current.push('`');
                    current.push_str(&code_text);
                }
            }
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // consume second `*`
                // Flush accumulated plain text.
                if !current.is_empty() {
                    segments.push((current.clone(), base_style));
                    current.clear();
                }
                let mut bold_text = String::new();
                let mut closed = false;
                while let Some(next) = chars.next() {
                    if next == '*' && chars.peek() == Some(&'*') {
                        chars.next(); // consume closing `*`
                        closed = true;
                        break;
                    }
                    bold_text.push(next);
                }
                if closed && !bold_text.is_empty() {
                    segments.push((bold_text, bold_style));
                } else {
                    // Unclosed bold — emit markers and text as plain.
                    current.push_str("**");
                    current.push_str(&bold_text);
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        segments.push((current, base_style));
    }

    if segments.is_empty() {
        segments.push((String::new(), base_style));
    }

    segments
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

    // -- Display line cache ---------------------------------------------------
    /// Pre-computed display lines for rendering.
    display_lines: Vec<DisplayLine>,
    /// Width used for the last display line computation.
    cached_width: u16,
    /// Flag to indicate display lines need recomputation.
    display_dirty: bool,
    /// Whether the view is pinned to the bottom (auto-scrolls on new content).
    pinned_to_bottom: bool,
    /// Optional code context from visual selection (one-shot, cleared after first message).
    context_code: Option<String>,
    /// Description of the context (e.g., "src/main.rs:42-58").
    context_info: Option<String>,
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
            display_lines: Vec::new(),
            cached_width: 0,
            display_dirty: true,
            pinned_to_bottom: true,
            context_code: None,
            context_info: None,
        }
    }

    /// Configure the overlay to target a specific agent.
    pub fn with_agent(mut self, id: String, name: String) -> Self {
        self.agent_id = Some(id);
        self.agent_name = name;
        self
    }

    /// Attach code selection context to prepend to the first message.
    pub fn with_context(mut self, code: String, info: String) -> Self {
        self.context_code = Some(code);
        self.context_info = Some(info);
        self
    }

    /// Append a complete message to the conversation.
    pub fn push_message(&mut self, role: &str, content: &str) {
        self.messages.push(ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        });
        self.display_dirty = true;
    }

    /// Append text to the last assistant message, or create one if needed.
    pub fn append_to_last(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut() {
            if last.role == "assistant" {
                last.content.push_str(text);
                self.display_dirty = true;
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

    /// Count the total display lines (after word wrapping).
    fn total_output_lines(&self) -> usize {
        self.display_lines.len()
    }

    /// Recompute the display line cache from the current messages.
    ///
    /// Called lazily at the start of `render()` when the cache is dirty
    /// or the available width has changed.  Assistant messages are parsed
    /// for markdown-like formatting (headers, bold, inline code, fenced
    /// code blocks, list items).  User messages are rendered plain.
    fn recompute_display_lines(
        &mut self,
        width: u16,
        user_style: Style,
        assistant_style: Style,
        _role_label_style: Style,
        code_style: Style,
        bold_style: Style,
        inline_code_style: Style,
        system_style: Style,
        user_header_style: Style,
        assistant_header_style: Style,
        system_header_style: Style,
        error_style: Style,
        error_header_style: Style,
    ) {
        self.display_lines.clear();

        let content_width = width.saturating_sub(2) as usize;
        let user_content_width = width.saturating_sub(3) as usize; // Extra indent for accent bar

        for msg in &self.messages {
            // -- Role header --------------------------------------------------
            let (label, label_style) = match msg.role.as_str() {
                "user" => ("┃ You".to_string(), user_header_style),
                "system" => ("⚙ System".to_string(), system_header_style),
                "error" => ("✗ Error".to_string(), error_header_style),
                _ => ("● Assistant".to_string(), assistant_header_style),
            };
            self.display_lines.push(DisplayLine::RoleHeader {
                text: label,
                style: label_style,
            });

            // -- Content lines (word-wrapped) ---------------------------------
            let is_user = msg.role == "user";
            let is_system = msg.role == "system";
            let is_error = msg.role == "error";
            let content_style = if is_user {
                user_style
            } else if is_system {
                system_style
            } else if is_error {
                error_style
            } else {
                assistant_style
            };

            if msg.content.is_empty() {
                self.display_lines.push(DisplayLine::BlankLine);
            } else {
                // Track fenced code block state across lines within a message.
                let mut in_code_block = false;

                for raw_line in msg.content.lines() {
                    if is_user || is_system || is_error {
                        // User/system/error messages: plain text with word wrapping, no markdown.
                        let wrap_width = if is_user { user_content_width } else { content_width };
                        for wrapped_line in word_wrap(raw_line, wrap_width) {
                            self.display_lines.push(DisplayLine::Content {
                                segments: vec![(wrapped_line, content_style)],
                                is_code_block: false,
                                is_user,
                            });
                        }
                    } else if in_code_block {
                        // Don't word-wrap code blocks — preserve indentation
                        // and formatting. Truncation happens at render time
                        // via set_stringn.
                        let segments = parse_markdown_segments(
                            raw_line,
                            &mut in_code_block,
                            content_style,
                            code_style,
                            bold_style,
                            inline_code_style,
                        );
                        self.display_lines.push(DisplayLine::Content {
                            segments,
                            is_code_block: true,
                            is_user: false,
                        });
                    } else {
                        // Normal assistant text: word-wrap then parse markdown.
                        for wrapped_line in word_wrap(raw_line, content_width) {
                            let was_in_code_block = in_code_block;
                            let segments = parse_markdown_segments(
                                &wrapped_line,
                                &mut in_code_block,
                                content_style,
                                code_style,
                                bold_style,
                                inline_code_style,
                            );
                            self.display_lines.push(DisplayLine::Content {
                                segments,
                                is_code_block: was_in_code_block || in_code_block,
                                is_user: false,
                            });
                        }
                    }
                }
            }

            // -- Blank separator between messages -----------------------------
            self.display_lines.push(DisplayLine::BlankLine);
        }

        self.cached_width = width;
        self.display_dirty = false;
    }

    /// Render the message history into the output area using the
    /// pre-computed display line cache.
    fn render_messages(
        &self,
        area: Rect,
        surface: &mut Surface,
        theme: &Theme,
        _role_label_style: Style,
    ) {
        let max_y = area.y + area.height;
        let visible_height = area.height as usize;
        let mut y = area.y;

        for (line_idx, line) in self.display_lines.iter().enumerate() {
            // Skip lines before the scroll offset.
            if line_idx < self.scroll_offset {
                continue;
            }
            // Stop once we've filled the visible area.
            if line_idx >= self.scroll_offset + visible_height {
                break;
            }

            let line_y = y;
            if line_y >= max_y {
                break;
            }

            match line {
                DisplayLine::RoleHeader { text, style } => {
                    surface.set_stringn(area.x, line_y, text, area.width as usize, *style);
                }
                DisplayLine::Content {
                    segments,
                    is_code_block,
                    is_user,
                } => {
                    if *is_code_block {
                        // Fill the full line width with a dark background.
                        let code_bg = theme.get("ui.statusline");
                        for x in area.x..(area.x + area.width) {
                            if let Some(cell) = surface.get_mut(x, line_y) {
                                cell.set_style(code_bg);
                            }
                        }
                        // Draw a gutter bar on the left edge.
                        if let Some(cell) = surface.get_mut(area.x, line_y) {
                            cell.set_char('▎');
                            cell.set_style(theme.get("string"));
                        }
                    }

                    // Draw left accent bar for user messages.
                    if *is_user && !*is_code_block {
                        let accent_style = theme.get("diff.plus");
                        if let Some(cell) = surface.get_mut(area.x + 1, line_y) {
                            cell.set_char('┃');
                            cell.set_style(accent_style);
                        }
                    }

                    let indent: u16 = if *is_user { 3 } else { 2 };
                    let mut x = area.x + indent;
                    for (text, style) in segments {
                        let max_chars =
                            (area.x + area.width).saturating_sub(x) as usize;
                        if max_chars == 0 {
                            break;
                        }
                        surface.set_stringn(x, line_y, text, max_chars, *style);
                        x += UnicodeWidthStr::width(text.as_str()) as u16;
                    }
                    // Streaming cursor: show ▍ at end of last content line while loading.
                    if self.loading && line_idx == self.display_lines.len().saturating_sub(2) {
                        let cursor_x = x;
                        if cursor_x < area.x + area.width {
                            let cursor_style = theme.get("ui.cursor.primary");
                            surface.set_stringn(cursor_x, line_y, "▍", 1, cursor_style);
                        }
                    }
                }
                DisplayLine::BlankLine => {
                    // Empty line — nothing to draw.
                }
            }

            y += 1;
        }

        // Loading indicator after all display lines.
        if self.loading && y < max_y {
            let content_width = area.width.saturating_sub(2) as usize;
            let thinking_style = theme.get("diff.plus");
            surface.set_stringn(
                area.x + 2,
                y,
                "◌ ◌ ◌  Thinking…",
                content_width,
                thinking_style,
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

        // Show context info as a system message on first render.
        if let Some(info) = &self.context_info {
            if self.messages.is_empty() {
                let msg = format!("Selected code from {}", info);
                self.push_message("system", &msg);
            }
        }

        let theme = &cx.editor.theme;

        // -- Background -------------------------------------------------------
        let bg_style = theme.get("ui.background");
        surface.clear_with(area, bg_style);

        // -- Outer border -----------------------------------------------------
        let border_style = theme.get("ui.popup.info");
        let pending_count = cx.editor.permission_queue.len();
        let title = if self.loading && pending_count > 0 {
            format!(" {} (loading…) ({} pending) ", self.agent_name, pending_count)
        } else if self.loading {
            format!(" {} (loading…) ", self.agent_name)
        } else if pending_count > 0 {
            format!(" {} ({} pending) ", self.agent_name, pending_count)
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

        // Markdown formatting styles for assistant messages.
        let code_style = assistant_style.patch(theme.get("ui.statusline"));
        let bold_style = assistant_style.add_modifier(Modifier::BOLD);
        let inline_code_style = assistant_style.patch(theme.get("ui.virtual.inlay-hint"));

        // Role header and message type styles.
        let system_style = theme.get("diagnostic.warning").add_modifier(Modifier::ITALIC);
        let user_header_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let assistant_header_style = theme.get("markup.heading").add_modifier(Modifier::BOLD);
        let system_header_style = theme.get("diagnostic.warning").add_modifier(Modifier::BOLD | Modifier::ITALIC);
        let error_style = theme.get("diagnostic.error");
        let error_header_style = error_style.add_modifier(Modifier::BOLD);

        // -- Recompute display line cache if needed ---------------------------
        if self.display_dirty || self.cached_width != output_area.width {
            self.recompute_display_lines(
                output_area.width,
                user_style,
                assistant_style,
                role_label_style,
                code_style,
                bold_style,
                inline_code_style,
                system_style,
                user_header_style,
                assistant_header_style,
                system_header_style,
                error_style,
                error_header_style,
            );
        }

        // -- Auto-scroll when pinned to bottom to keep latest content visible --
        if self.pinned_to_bottom {
            let total = self.total_output_lines();
            let visible = output_area.height as usize;
            if total > visible {
                self.scroll_offset = total.saturating_sub(visible);
            } else {
                self.scroll_offset = 0;
            }
        }

        // -- Clamp scroll_offset to valid range -------------------------------
        {
            let total = self.display_lines.len();
            let visible = output_area.height as usize;
            let max_offset = total.saturating_sub(visible);
            if self.scroll_offset > max_offset {
                self.scroll_offset = max_offset;
            }
        }

        // -- Render messages --------------------------------------------------
        self.render_messages(
            output_area,
            surface,
            theme,
            role_label_style,
        );

        // -- Scroll indicators ------------------------------------------------
        {
            let total = self.display_lines.len();
            let visible = output_area.height as usize;
            let dim_style = theme.get("ui.virtual.whitespace");

            // Top indicator: "▲ N more" when scrolled down
            if self.scroll_offset > 0 {
                let indicator = format!("▲ {} more", self.scroll_offset);
                let max_chars = output_area.width as usize;
                surface.set_stringn(
                    output_area.x,
                    output_area.y,
                    &indicator,
                    max_chars,
                    dim_style,
                );
            }

            // Bottom indicator: "▼ N more" when content extends below
            let visible_end = self.scroll_offset + visible;
            if visible_end < total {
                let remaining = total - visible_end;
                let indicator = format!("▼ {} more", remaining);
                let max_chars = output_area.width as usize;
                let bottom_y = output_area.y + output_area.height.saturating_sub(1);
                surface.set_stringn(
                    output_area.x,
                    bottom_y,
                    &indicator,
                    max_chars,
                    dim_style,
                );
            }
        }

        // -- Input separator line ---------------------------------------------
        let sep_y = input_area.y;
        for x in input_area.x..input_area.x + input_area.width {
            surface.set_stringn(x, sep_y, "─", 1, border_style);
        }

        // -- Input prompt -----------------------------------------------------
        let prompt_y = sep_y + 1;

        surface.set_stringn(input_area.x, prompt_y, "❯ ", 2, prompt_style);
        if self.input.is_empty() && pending_count > 0 {
            let hint = "Enter to review pending edit │ type to chat";
            let hint_style = theme.get("ui.virtual.whitespace");
            surface.set_stringn(
                input_area.x + 2,
                prompt_y,
                &hint,
                input_area.width.saturating_sub(2) as usize,
                hint_style,
            );
        } else if self.input.is_empty() && !self.loading {
            let hint_style = theme.get("ui.virtual.whitespace");
            surface.set_stringn(
                input_area.x + 2,
                prompt_y,
                "Type a message…",
                input_area.width.saturating_sub(2) as usize,
                hint_style,
            );
        } else {
            surface.set_stringn(
                input_area.x + 2,
                prompt_y,
                &self.input,
                input_area.width.saturating_sub(2) as usize,
                text_style,
            );
        }
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
                // If input is empty and there are pending permissions, open review
                if self.input.is_empty() {
                    let has_pending = !cx.editor.permission_queue.is_empty();
                    if has_pending {
                        let server = match &cx.editor.opencode_server {
                            Some(s) => s,
                            None => return EventResult::Consumed(None),
                        };
                        let client = server.client().clone();
                        let request = cx.editor.permission_queue.pop_front().unwrap();
                        let queue_total = cx.editor.permission_queue.len() + 1;

                        let view = crate::ui::permission_view::PermissionDiffView::new(
                            &request,
                            client,
                            1,
                            queue_total,
                        )
                        .with_from_overlay();

                        let callback: Callback = Box::new(move |compositor, _cx| {
                            compositor.push(Box::new(crate::ui::overlay::overlaid(view)));
                        });
                        return EventResult::Consumed(Some(callback));
                    }
                }

                if !self.input.is_empty() && !self.loading {
                    let mut message = self.input.clone();
                    self.input.clear();
                    self.input_cursor = 0;

                    // Prepend code context to the first message (one-shot).
                    if let Some(code) = self.context_code.take() {
                        let info = self.context_info.take().unwrap_or_default();
                        message = format!(
                            "Context from {}:\n```\n{}\n```\n\n{}",
                            info, code, message
                        );
                    }

                    self.push_message("user", &message);
                    self.loading = true;
                    self.pinned_to_bottom = true;

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
                                let client_for_perms = client.clone();

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
                                        "permission.asked" => {
                                            if let Ok(perm_request) = serde_json::from_value::<
                                                helix_opencode::types::PermissionRequest,
                                            >(
                                                event.properties
                                            ) {
                                                if perm_request.session_id == session_id_clone {
                                                    let display_name =
                                                        perm_request.display_name();
                                                    let perm_id = perm_request.id.clone();
                                                    let perm_client =
                                                        client_for_perms.clone();
                                                    let flag = cancelled.clone();

                                                    job::dispatch(
                                                        move |editor, compositor| {
                                                            match editor.permission_mode {
                                                                helix_view::editor::PermissionMode::AutoApprove => {
                                                                    editor.set_status(format!(
                                                                        "Auto-approved: {}",
                                                                        display_name
                                                                    ));
                                                                    let pid = perm_id.clone();
                                                                    tokio::spawn(async move {
                                                                        if let Err(e) = perm_client
                                                                            .reply_permission(
                                                                                &pid, "once", None,
                                                                            )
                                                                            .await
                                                                        {
                                                                            log::error!("Auto-approve failed: {}", e);
                                                                        }
                                                                    });
                                                                    if let Some(overlay) = compositor
                                                                        .find::<Overlay<AgentOverlay>>()
                                                                    {
                                                                        overlay.content.push_message(
                                                                            "system",
                                                                            &format!(
                                                                                "Auto-approved: {}",
                                                                                display_name
                                                                            ),
                                                                        );
                                                                    } else {
                                                                        flag.store(
                                                                            true,
                                                                            std::sync::atomic::Ordering::Relaxed,
                                                                        );
                                                                    }
                                                                }
                                                                helix_view::editor::PermissionMode::AutoReject => {
                                                                    editor.set_status(format!(
                                                                        "Auto-rejected: {}",
                                                                        display_name
                                                                    ));
                                                                    let pid = perm_id.clone();
                                                                    tokio::spawn(async move {
                                                                        if let Err(e) = perm_client
                                                                            .reply_permission(
                                                                                &pid, "reject", None,
                                                                            )
                                                                            .await
                                                                        {
                                                                            log::error!("Auto-reject failed: {}", e);
                                                                        }
                                                                    });
                                                                    if let Some(overlay) = compositor
                                                                        .find::<Overlay<AgentOverlay>>()
                                                                    {
                                                                        overlay.content.push_message(
                                                                            "system",
                                                                            &format!(
                                                                                "Auto-rejected: {}",
                                                                                display_name
                                                                            ),
                                                                        );
                                                                    } else {
                                                                        flag.store(
                                                                            true,
                                                                            std::sync::atomic::Ordering::Relaxed,
                                                                        );
                                                                    }
                                                                }
                                                                helix_view::editor::PermissionMode::Ask => {
                                                                    let pending_display = display_name.clone();
                                                                    editor
                                                                        .permission_queue
                                                                        .push_back(perm_request);
                                                                    let pending =
                                                                        editor.permission_queue.len();
                                                                    if let Some(overlay) = compositor
                                                                        .find::<Overlay<AgentOverlay>>()
                                                                    {
                                                                        overlay.content.push_message(
                                                                            "system",
                                                                            &format!(
                                                                                "Agent wants to edit {} — press Esc then :ai-perm to review ({} pending)",
                                                                                pending_display, pending
                                                                            ),
                                                                        );
                                                                    } else {
                                                                        flag.store(
                                                                            true,
                                                                            std::sync::atomic::Ordering::Relaxed,
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        },
                                                    )
                                                    .await;
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
                                        agent.pinned_to_bottom = true;
                                    },
                                )))
                            }
                        }
                    });
                }
                EventResult::Consumed(None)
            }

            // -- Scroll output (Ctrl+k / Ctrl+j) ---------------------------------
            KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                self.pinned_to_bottom = false;
                EventResult::Consumed(None)
            }
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                // If we've scrolled to the bottom, re-pin
                // (exact clamping happens in render, so use a heuristic:
                //  the total lines are at least as many as display_lines)
                let total = self.display_lines.len();
                // We don't know visible height here, so just mark pinned
                // if offset is clearly past the end; render() will clamp.
                if self.scroll_offset + 1 >= total {
                    self.pinned_to_bottom = true;
                }
                EventResult::Consumed(None)
            }

            // -- Jump to bottom (G) -------------------------------------------
            KeyCode::Char('G') if key.modifiers == KeyModifiers::SHIFT => {
                self.pinned_to_bottom = true;
                // scroll_offset will be adjusted in render()
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
            KeyCode::End if key.modifiers == KeyModifiers::CONTROL => {
                // Ctrl+End: jump to bottom of output
                self.pinned_to_bottom = true;
                EventResult::Consumed(None)
            }
            KeyCode::End => {
                self.input_cursor = self.input.len();
                EventResult::Consumed(None)
            }

            // -- Scroll output (PageUp / PageDown) ----------------------------
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                self.pinned_to_bottom = false;
                EventResult::Consumed(None)
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(20);
                // If scrolled past the end, pin to bottom (render will clamp)
                let total = self.display_lines.len();
                if self.scroll_offset + 1 >= total {
                    self.pinned_to_bottom = true;
                }
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

        let display_col = UnicodeWidthStr::width(&self.input[..self.input_cursor]) as u16;

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
