// Permission review diff view — shows proposed agent edits for approval/rejection.
//
// Provides unified diff parsing and before/after content reconstruction so that
// agent-proposed changes can be displayed using the same DiffView infrastructure.
//
// Also contains `PermissionDiffView`, a full-screen Component that renders a
// colored unified diff with approve/reject/feedback actions.

use crate::compositor::{Callback, Component, Compositor, Context, Event, EventResult};
use crate::job;
use crate::ui::diff_view::{
    compute_diff_lines_from_hunks, compute_word_diff, render_diff_line_simple, should_pair_lines,
    DiffLine,
};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::{Position, Rope};
use helix_vcs::Hunk;
use helix_view::graphics::{CursorKind, Modifier, Rect};
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::Editor;
use imara_diff::{Algorithm, Diff, InternedInput};
use tui::buffer::Buffer as Surface;
use tui::text::Span;
use tui::widgets::{Block, Widget};

/// Parsed info from a unified diff hunk header.
#[derive(Debug, Clone)]
pub struct DiffHunkInfo {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    /// Lines in this hunk (context, additions, deletions).
    pub lines: Vec<DiffLineType>,
}

/// Classification of a single line within a unified diff hunk.
#[derive(Debug, Clone)]
pub enum DiffLineType {
    Context(String),
    Addition(String),
    Deletion(String),
}

/// Parse a unified diff `@@ -X,Y +A,B @@` header line into (old_start, old_count, new_start, new_count).
///
/// Handles formats:
/// - `@@ -X,Y +A,B @@`           — standard
/// - `@@ -X +A @@`               — implied count=1
/// - `@@ -X,0 +A,B @@`          — pure addition
/// - `@@ -X,Y +A,0 @@`          — pure deletion
/// - `@@ -X,Y +A,B @@ context`  — with function context after @@
fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    // Strip leading "@@" and find the closing "@@"
    let trimmed = line.strip_prefix("@@")?.trim_start();
    let end_marker = trimmed.find("@@")?;
    let range_part = trimmed[..end_marker].trim();

    // Split into old and new range parts: "-X,Y +A,B" or "-X +A"
    let mut parts = range_part.split_whitespace();

    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;

    let (old_start, old_count) = parse_range_spec(old_part)?;
    let (new_start, new_count) = parse_range_spec(new_part)?;

    Some((old_start, old_count, new_start, new_count))
}

/// Parse a range spec like "X,Y" or "X" (implied count=1) into (start, count).
fn parse_range_spec(spec: &str) -> Option<(usize, usize)> {
    if let Some((start_str, count_str)) = spec.split_once(',') {
        let start = start_str.parse::<usize>().ok()?;
        let count = count_str.parse::<usize>().ok()?;
        Some((start, count))
    } else {
        let start = spec.parse::<usize>().ok()?;
        // No comma means count is implicitly 1
        Some((start, 1))
    }
}

/// Parse a unified diff string into a list of `DiffHunkInfo` structs.
///
/// Skips `diff --git`, `index`, `---`, `+++` header lines.
/// For each `@@ -X,Y +A,B @@` line, creates a new hunk and classifies
/// subsequent lines as Context (` `), Addition (`+`), or Deletion (`-`).
pub fn parse_diff_hunks(diff: &str) -> Vec<DiffHunkInfo> {
    let mut hunks = Vec::new();
    let mut current_hunk: Option<DiffHunkInfo> = None;

    for line in diff.lines() {
        // Skip diff header lines
        if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("similarity index ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("Binary files ")
        {
            continue;
        }

        // Check for hunk header
        if line.starts_with("@@") {
            // Save previous hunk if any
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }

            if let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(line) {
                current_hunk = Some(DiffHunkInfo {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                    lines: Vec::new(),
                });
            } else {
                log::warn!("Failed to parse hunk header: {}", line);
            }
            continue;
        }

        // Classify lines within a hunk
        if let Some(ref mut hunk) = current_hunk {
            if let Some(content) = line.strip_prefix('+') {
                hunk.lines.push(DiffLineType::Addition(content.to_string()));
            } else if let Some(content) = line.strip_prefix('-') {
                hunk.lines.push(DiffLineType::Deletion(content.to_string()));
            } else if let Some(content) = line.strip_prefix(' ') {
                hunk.lines.push(DiffLineType::Context(content.to_string()));
            } else if line == "\\ No newline at end of file" {
                // Git marker for missing trailing newline — skip
            } else {
                // Treat unrecognized lines as context (some diffs omit the leading space)
                hunk.lines.push(DiffLineType::Context(line.to_string()));
            }
        }
    }

    // Push the last hunk
    if let Some(hunk) = current_hunk {
        hunks.push(hunk);
    }

    hunks
}

/// Reconstruct before and after file content from a unified diff.
///
/// Returns `(before_content, after_content)` where:
/// - `before_content` is the original file (deletions + context)
/// - `after_content` is the modified file (additions + context)
///
/// Gaps between hunks (lines not covered by any hunk) are treated as identical
/// in both versions. If parsing fails entirely, returns `("", raw_diff)` as fallback.
pub fn reconstruct_from_diff(diff: &str) -> (String, String) {
    let hunks = parse_diff_hunks(diff);

    if hunks.is_empty() {
        log::warn!("reconstruct_from_diff: no hunks found, returning fallback");
        return (String::new(), diff.to_string());
    }

    let mut before_lines: Vec<String> = Vec::new();
    let mut after_lines: Vec<String> = Vec::new();

    // Track current position in the old file (1-indexed, matching diff line numbers)
    let mut old_pos: usize = 1;

    for hunk in &hunks {
        // Fill gap between previous position and this hunk's start.
        // Lines in the gap are identical in both before and after, but we don't
        // have their content. We insert placeholder empty lines to keep line
        // numbers aligned. However, for a complete diff (covering the whole file),
        // there should be no gaps or the context lines cover them.
        if hunk.old_start > old_pos {
            let gap_count = hunk.old_start - old_pos;
            for _ in 0..gap_count {
                // We don't know the actual content of gap lines, so insert empty
                // placeholders. This keeps line numbering correct for imara_diff.
                before_lines.push(String::new());
                after_lines.push(String::new());
            }
        }

        // Process hunk lines
        let mut hunk_old_consumed = 0;
        let mut hunk_new_consumed = 0;

        for line_type in &hunk.lines {
            match line_type {
                DiffLineType::Context(content) => {
                    before_lines.push(content.clone());
                    after_lines.push(content.clone());
                    hunk_old_consumed += 1;
                    hunk_new_consumed += 1;
                }
                DiffLineType::Deletion(content) => {
                    before_lines.push(content.clone());
                    hunk_old_consumed += 1;
                }
                DiffLineType::Addition(content) => {
                    after_lines.push(content.clone());
                    hunk_new_consumed += 1;
                }
            }
        }

        // Advance old_pos past this hunk
        old_pos = hunk.old_start + hunk_old_consumed;
    }

    // Join lines with newlines and add trailing newline for consistency
    let before = join_lines(&before_lines);
    let after = join_lines(&after_lines);

    (before, after)
}

/// Join lines into a single string with newline separators.
/// Produces a trailing newline if there are any lines, matching typical file content.
fn join_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Wrapper for `RopeSlice` to implement `imara_diff::TokenSource`.
struct RopeLines<'a>(helix_core::RopeSlice<'a>);

impl<'a> imara_diff::TokenSource for RopeLines<'a> {
    type Token = helix_core::RopeSlice<'a>;
    type Tokenizer = helix_core::ropey::iter::Lines<'a>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.lines()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len_lines() as u32
    }
}

/// Compute diff hunks between two `Rope` contents using `imara_diff`.
///
/// Returns the same `Hunk` type used by `DiffView`, enabling seamless integration
/// with the existing diff rendering infrastructure.
pub fn compute_hunks(before: &Rope, after: &Rope) -> Vec<Hunk> {
    let input = InternedInput::new(RopeLines(before.slice(..)), RopeLines(after.slice(..)));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    diff.hunks().collect()
}

/// End-to-end helper: parse a unified diff string and compute `imara_diff::Hunk`s.
///
/// 1. Reconstructs before/after content from the diff text
/// 2. Computes hunks via `imara_diff` for use with `DiffView`
///
/// Returns `(before_rope, after_rope, hunks)`.
pub fn parse_and_compute_hunks(diff: &str) -> (Rope, Rope, Vec<Hunk>) {
    let (before_text, after_text) = reconstruct_from_diff(diff);
    let before_rope = Rope::from(before_text);
    let after_rope = Rope::from(after_text);
    let hunks = compute_hunks(&before_rope, &after_rope);
    (before_rope, after_rope, hunks)
}

// =============================================================================
// PermissionDiffView — full-screen diff review for agent edits
// =============================================================================

/// A single line in the rendered diff.
enum PermDiffLine {
    /// Hunk header (e.g. `@@ -10,5 +20,7 @@ fn main`)
    Header(String),
    /// Unchanged context line
    Context(String),
    /// Added line
    Addition(String),
    /// Removed line
    Deletion(String),
}

/// Full-screen component that displays a colored unified diff for an agent's
/// proposed edit, with keyboard shortcuts to approve, reject, or provide
/// feedback before rejecting.
pub struct PermissionDiffView {
    /// Permission request ID used when replying to the server.
    permission_id: String,
    /// File being edited (displayed in the header).
    file_path: String,
    /// Queue position (1-indexed) when multiple permissions are pending.
    queue_position: usize,
    /// Total number of pending permissions.
    queue_total: usize,
    /// Pre-parsed diff lines for rendering (using DiffView's DiffLine for rich rendering).
    diff_lines: Vec<DiffLine>,
    /// Vertical scroll offset into `diff_lines`.
    scroll_offset: usize,
    /// Number of added lines in the diff.
    additions: usize,
    /// Number of removed lines in the diff.
    deletions: usize,
    /// When `true`, the footer shows a text input for rejection feedback.
    feedback_mode: bool,
    /// Text the user is typing as rejection feedback.
    feedback_input: String,
    /// Byte-offset cursor position within `feedback_input`.
    feedback_cursor: usize,
    /// OpenCode HTTP client for sending the reply.
    client: helix_opencode::client::OpenCodeClient,
    /// Original permission request, kept so Esc can re-queue it.
    request: helix_opencode::types::PermissionRequest,
    /// Whether this view was opened from the agent overlay (affects Esc hint).
    from_overlay: bool,
}

impl PermissionDiffView {
    pub fn new(
        request: &helix_opencode::types::PermissionRequest,
        client: helix_opencode::client::OpenCodeClient,
        queue_position: usize,
        queue_total: usize,
    ) -> Self {
        let file_path = request.display_name();
        let diff_text = request.diff().unwrap_or_default();

        // Parse the unified diff into before/after ropes and compute DiffLines
        let (before_rope, after_rope, hunks) = parse_and_compute_hunks(&diff_text);
        let (diff_lines, _boundaries) = compute_diff_lines_from_hunks(&before_rope, &after_rope, &hunks);

        // Count additions and deletions
        let mut additions = 0usize;
        let mut deletions = 0usize;
        for line in &diff_lines {
            match line {
                DiffLine::Addition { .. } => additions += 1,
                DiffLine::Deletion { .. } => deletions += 1,
                _ => {}
            }
        }

        Self {
            permission_id: request.id.clone(),
            file_path,
            queue_position,
            queue_total,
            diff_lines,
            scroll_offset: 0,
            additions,
            deletions,
            feedback_mode: false,
            feedback_input: String::new(),
            feedback_cursor: 0,
            client,
            request: request.clone(),
            from_overlay: false,
        }
    }

    /// Mark this view as opened from the agent overlay chat.
    pub fn with_from_overlay(mut self) -> Self {
        self.from_overlay = true;
        self
    }

    /// Parse a unified diff string into colored display lines, returning
    /// `(lines, addition_count, deletion_count)`.
    fn parse_diff_lines(diff_text: &str) -> (Vec<PermDiffLine>, usize, usize) {
        let mut lines = Vec::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;

        for line in diff_text.lines() {
            if line.starts_with("@@") {
                lines.push(PermDiffLine::Header(line.to_string()));
            } else if line.starts_with('+') && !line.starts_with("+++") {
                additions += 1;
                // Strip the leading '+' for display; we render our own gutter.
                lines.push(PermDiffLine::Addition(line[1..].to_string()));
            } else if line.starts_with('-') && !line.starts_with("---") {
                deletions += 1;
                lines.push(PermDiffLine::Deletion(line[1..].to_string()));
            } else if line.starts_with(' ') {
                lines.push(PermDiffLine::Context(line[1..].to_string()));
            } else if line.starts_with("diff ")
                || line.starts_with("index ")
                || line.starts_with("---")
                || line.starts_with("+++")
            {
                // Skip metadata headers
            } else {
                // Treat anything else (e.g. missing-newline marker) as context
                lines.push(PermDiffLine::Context(line.to_string()));
            }
        }

        (lines, additions, deletions)
    }

    /// Maximum number of diff lines that can scroll past the bottom.
    fn max_scroll(&self, visible_height: usize) -> usize {
        self.diff_lines.len().saturating_sub(visible_height)
    }

    /// Send an async reply to the OpenCode server and pop this view.
    fn reply_and_close(
        &self,
        reply: &'static str,
        message: Option<String>,
        status_msg: String,
    ) -> EventResult {
        let permission_id = self.permission_id.clone();
        let client = self.client.clone();

        let callback: Callback = Box::new(move |compositor: &mut Compositor, cx: &mut Context| {
            compositor.pop();
            cx.editor.set_status(status_msg);

            cx.jobs.callback(async move {
                let msg_ref = message.as_deref();
                if let Err(e) = client
                    .reply_permission(&permission_id, reply, msg_ref)
                    .await
                {
                    log::error!("Failed to send permission reply: {}", e);
                    crate::job::dispatch(move |editor, _| {
                        editor.set_error(format!("Permission reply failed: {}", e));
                    })
                    .await;
                }
                Ok(job::Callback::EditorCompositor(Box::new(|_, _| {})))
            });
        });
        EventResult::Consumed(Some(callback))
    }
}

impl Component for PermissionDiffView {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;

        // -- Background -------------------------------------------------------
        let bg_style = theme.get("ui.background");
        surface.clear_with(area, bg_style);

        // -- Outer border -----------------------------------------------------
        let border_style = theme.get("ui.popup.info");
        let title = format!(" Agent Edit: {} ", self.file_path);
        let block = Block::bordered()
            .title(Span::styled(title, border_style))
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, surface);

        if inner.width < 4 || inner.height < 6 {
            return;
        }

        // -- Layout -----------------------------------------------------------
        // header_height: stats line + separator = 2
        // footer_height: separator + hints (or feedback) = 2 or 3
        let header_height = 2u16;
        let footer_height: u16 = if self.feedback_mode { 3 } else { 2 };

        let header_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: header_height,
        };
        let diff_area = Rect {
            x: inner.x,
            y: inner.y + header_height,
            width: inner.width,
            height: inner.height.saturating_sub(header_height + footer_height),
        };
        let footer_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(footer_height),
            width: inner.width,
            height: footer_height,
        };

        // -- Styles -----------------------------------------------------------
        let title_style = theme
            .get("ui.text.focus")
            .add_modifier(Modifier::BOLD);
        let dim_style = theme.get("ui.virtual.whitespace");
        let text_style = theme.get("ui.text");

        // -- Header: stats line -----------------------------------------------
        let queue_info = if self.queue_total > 1 {
            format!(" ({}/{})", self.queue_position, self.queue_total)
        } else {
            String::new()
        };
        let stats = format!(
            "+{} -{}{queue_info}",
            self.additions, self.deletions
        );
        surface.set_stringn(
            header_area.x,
            header_area.y,
            &stats,
            header_area.width as usize,
            title_style,
        );

        // Separator
        let sep: String = "─".repeat(header_area.width as usize);
        surface.set_stringn(
            header_area.x,
            header_area.y + 1,
            &sep,
            header_area.width as usize,
            dim_style,
        );

        // -- Diff lines (using DiffView's rich rendering) ---------------------
        let visible_height = diff_area.height as usize;
        let line_number_width = 10u16;
        let style_minus_emph = theme.get("diff.minus").add_modifier(Modifier::BOLD).underline_style(helix_view::graphics::UnderlineStyle::Line);
        let style_plus_emph = theme.get("diff.plus").add_modifier(Modifier::BOLD).underline_style(helix_view::graphics::UnderlineStyle::Line);

        // Pre-compute word-diff pairs: for each Deletion[i] followed by Addition[i+1],
        // check if they should be paired for word-level highlighting.
        let mut word_diff_pairs: std::collections::HashMap<usize, Vec<crate::ui::diff_view::WordSegment>> =
            std::collections::HashMap::new();
        for i in 0..self.diff_lines.len().saturating_sub(1) {
            if let (DiffLine::Deletion { content: old, .. }, DiffLine::Addition { content: new, .. }) =
                (&self.diff_lines[i], &self.diff_lines[i + 1])
            {
                if should_pair_lines(old, new) {
                    let (old_segs, new_segs) = compute_word_diff(old, new);
                    word_diff_pairs.insert(i, old_segs);
                    word_diff_pairs.insert(i + 1, new_segs);
                }
            }
        }

        for (i, line) in self
            .diff_lines
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(visible_height)
        {
            let y = diff_area.y + (i - self.scroll_offset) as u16;

            // Check if this line has word-level diff segments
            if let Some(segments) = word_diff_pairs.get(&i) {
                // Render the base line first (background + gutter)
                render_diff_line_simple(line, y, diff_area, surface, theme, line_number_width);

                // Then overlay word-level emphasis on the content area
                let gutter_total = line_number_width as u16 + 4;
                let mut x = diff_area.x + gutter_total;
                let max_x = diff_area.x + diff_area.width;
                let is_deletion = matches!(line, DiffLine::Deletion { .. });
                let emph_style = if is_deletion { style_minus_emph } else { style_plus_emph };

                for seg in segments {
                    if x >= max_x {
                        break;
                    }
                    let seg_width = UnicodeWidthStr::width(seg.text.as_str()) as u16;
                    if seg.is_emph {
                        // Overwrite with emphasis style
                        let chars_available = (max_x - x) as usize;
                        surface.set_stringn(x, y, &seg.text, chars_available, emph_style);
                    }
                    x += seg_width;
                }
            } else {
                render_diff_line_simple(line, y, diff_area, surface, theme, line_number_width);
            }
        }

        // -- Scroll indicator (right edge) ------------------------------------
        if self.diff_lines.len() > visible_height {
            let max = self.max_scroll(visible_height);
            let pct = if max > 0 {
                (self.scroll_offset as f64 / max as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let indicator_y =
                diff_area.y + (pct * (diff_area.height.saturating_sub(1)) as f64) as u16;
            surface.set_stringn(
                diff_area.x + diff_area.width.saturating_sub(1),
                indicator_y,
                "█",
                1,
                dim_style,
            );
        }

        // -- Footer -----------------------------------------------------------
        surface.set_stringn(
            footer_area.x,
            footer_area.y,
            &sep,
            footer_area.width as usize,
            dim_style,
        );

        if self.feedback_mode {
            let prompt_style = theme.get("ui.text.focus");
            surface.set_stringn(
                footer_area.x,
                footer_area.y + 1,
                "Feedback: ",
                10,
                prompt_style,
            );
            surface.set_stringn(
                footer_area.x + 10,
                footer_area.y + 1,
                &self.feedback_input,
                (footer_area.width.saturating_sub(10)) as usize,
                text_style,
            );
            surface.set_stringn(
                footer_area.x,
                footer_area.y + 2,
                "[Enter] send  [Esc] cancel",
                footer_area.width as usize,
                dim_style,
            );
        } else {
            let hint = if self.from_overlay {
                "[a]pprove  [x]reject  [X]reject+feedback  [A]always  [Esc] back to chat"
            } else {
                "[a]pprove  [x]reject  [X]reject+feedback  [A]always  [Esc] defer"
            };
            surface.set_stringn(
                footer_area.x,
                footer_area.y + 1,
                hint,
                footer_area.width as usize,
                dim_style,
            );
        }
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        // -- Feedback input mode ----------------------------------------------
        if self.feedback_mode {
            match key.code {
                KeyCode::Esc => {
                    self.feedback_mode = false;
                    self.feedback_input.clear();
                    self.feedback_cursor = 0;
                    return EventResult::Consumed(None);
                }
                KeyCode::Enter => {
                    let feedback = self.feedback_input.clone();
                    let file_path = self.file_path.clone();
                    let status = format!("✗ Rejected edit to {} (with feedback)", file_path);
                    return self.reply_and_close("reject", Some(feedback), status);
                }
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.feedback_input.insert(self.feedback_cursor, c);
                    self.feedback_cursor += c.len_utf8();
                    return EventResult::Consumed(None);
                }
                KeyCode::Backspace => {
                    if self.feedback_cursor > 0 {
                        // Find the previous character boundary
                        let prev = self.feedback_input[..self.feedback_cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        self.feedback_input.remove(prev);
                        self.feedback_cursor = prev;
                    }
                    return EventResult::Consumed(None);
                }
                _ => return EventResult::Consumed(None),
            }
        }

        // -- Normal mode ------------------------------------------------------
        match key.code {
            KeyCode::Esc => {
                // Defer — go back without answering; push permission back to
                // the front of the queue so it can be reviewed later.
                let request = self.request.clone();
                let callback: Callback =
                    Box::new(move |compositor: &mut Compositor, cx: &mut Context| {
                        compositor.pop();
                        cx.editor.permission_queue.push_front(request);
                        let pending = cx.editor.permission_queue.len();
                        cx.editor.set_status(format!(
                            "Deferred — {} permission(s) pending",
                            pending
                        ));
                    });
                EventResult::Consumed(Some(callback))
            }

            // Approve (once)
            KeyCode::Char('a') if key.modifiers.is_empty() => {
                let status = format!("✓ Approved edit to {}", self.file_path);
                self.reply_and_close("once", None, status)
            }

            // Always approve (sets editor mode)
            KeyCode::Char('A') => {
                let permission_id = self.permission_id.clone();
                let client = self.client.clone();
                let file_path = self.file_path.clone();

                let callback: Callback =
                    Box::new(move |compositor: &mut Compositor, cx: &mut Context| {
                        compositor.pop();
                        cx.editor.permission_mode =
                            helix_view::editor::PermissionMode::AutoApprove;
                        cx.editor.set_status(format!(
                            "✓ Always approve — edit applied to {}",
                            file_path
                        ));

                        cx.jobs.callback(async move {
                            if let Err(e) = client
                                .reply_permission(&permission_id, "always", None)
                                .await
                            {
                                log::error!("Failed to send permission reply: {}", e);
                                crate::job::dispatch(move |editor, _| {
                                    editor.set_error(format!("Permission reply failed: {}", e));
                                })
                                .await;
                            }
                            Ok(job::Callback::EditorCompositor(Box::new(|_, _| {})))
                        });
                    });
                EventResult::Consumed(Some(callback))
            }

            // Reject (no feedback)
            KeyCode::Char('x') if key.modifiers.is_empty() => {
                let status = format!("✗ Rejected edit to {}", self.file_path);
                self.reply_and_close("reject", None, status)
            }

            // Reject with feedback
            KeyCode::Char('X') => {
                self.feedback_mode = true;
                EventResult::Consumed(None)
            }

            // -- Scrolling ----------------------------------------------------
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                // Clamp will happen naturally in render; but let's be precise
                let max = self.diff_lines.len().saturating_sub(1);
                self.scroll_offset = self.scroll_offset.min(max);
                EventResult::Consumed(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                EventResult::Consumed(None)
            }
            KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll_offset = self.scroll_offset.saturating_add(20);
                let max = self.diff_lines.len().saturating_sub(1);
                self.scroll_offset = self.scroll_offset.min(max);
                EventResult::Consumed(None)
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                EventResult::Consumed(None)
            }
            KeyCode::Char('G') => {
                self.scroll_offset = self.diff_lines.len().saturating_sub(1);
                EventResult::Consumed(None)
            }
            KeyCode::Char('g') if key.modifiers.is_empty() => {
                self.scroll_offset = 0;
                EventResult::Consumed(None)
            }

            // Consume everything else so keys don't leak to the editor
            _ => EventResult::Consumed(None),
        }
    }

    fn cursor(&self, area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        if self.feedback_mode {
            // Cursor sits in the feedback input line inside the border.
            // footer_area.y + 1 is the feedback line; x offset = border(1) + "Feedback: "(10)
            let footer_y = area.y + area.height.saturating_sub(3);
            let display_col =
                UnicodeWidthStr::width(&self.feedback_input[..self.feedback_cursor]) as u16;
            let cursor_x = area.x + 1 + 10 + display_col; // border + prompt
            (
                Some(Position::new(footer_y as usize, cursor_x as usize)),
                CursorKind::Block,
            )
        } else {
            (None, CursorKind::Hidden)
        }
    }

    fn required_size(&mut self, _viewport: (u16, u16)) -> Option<(u16, u16)> {
        // Full-screen — let the compositor decide the size.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hunk_header_standard() {
        let result = parse_hunk_header("@@ -10,5 +20,7 @@");
        assert_eq!(result, Some((10, 5, 20, 7)));
    }

    #[test]
    fn test_parse_hunk_header_implied_count() {
        let result = parse_hunk_header("@@ -10 +20 @@");
        assert_eq!(result, Some((10, 1, 20, 1)));
    }

    #[test]
    fn test_parse_hunk_header_with_context() {
        let result = parse_hunk_header("@@ -10,5 +20,7 @@ fn main() {");
        assert_eq!(result, Some((10, 5, 20, 7)));
    }

    #[test]
    fn test_parse_hunk_header_pure_addition() {
        let result = parse_hunk_header("@@ -10,0 +11,3 @@");
        assert_eq!(result, Some((10, 0, 11, 3)));
    }

    #[test]
    fn test_parse_hunk_header_pure_deletion() {
        let result = parse_hunk_header("@@ -10,3 +9,0 @@");
        assert_eq!(result, Some((10, 3, 9, 0)));
    }

    #[test]
    fn test_parse_diff_hunks_simple() {
        let diff = "\
diff --git a/foo.rs b/foo.rs
index abc1234..def5678 100644
--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"hello\");
+    println!(\"hello world\");
+    println!(\"goodbye\");
 }
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].old_count, 3);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].new_count, 4);
        assert_eq!(hunks[0].lines.len(), 5);
    }

    #[test]
    fn test_parse_diff_hunks_multiple() {
        let diff = "\
@@ -1,3 +1,3 @@
 line1
-line2
+line2_modified
 line3
@@ -10,2 +10,3 @@
 line10
+line10.5
 line11
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 10);
    }

    #[test]
    fn test_reconstruct_simple_modification() {
        let diff = "\
@@ -1,3 +1,3 @@
 fn main() {
-    println!(\"hello\");
+    println!(\"hello world\");
 }
";
        let (before, after) = reconstruct_from_diff(diff);
        assert!(before.contains("println!(\"hello\");"));
        assert!(!before.contains("hello world"));
        assert!(after.contains("println!(\"hello world\");"));
        assert!(!after.contains("println!(\"hello\");"));
    }

    #[test]
    fn test_reconstruct_pure_addition() {
        let diff = "\
@@ -1,2 +1,4 @@
 line1
+added1
+added2
 line2
";
        let (before, after) = reconstruct_from_diff(diff);
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        assert_eq!(before_lines, vec!["line1", "line2"]);
        assert_eq!(after_lines, vec!["line1", "added1", "added2", "line2"]);
    }

    #[test]
    fn test_reconstruct_pure_deletion() {
        let diff = "\
@@ -1,4 +1,2 @@
 line1
-removed1
-removed2
 line2
";
        let (before, after) = reconstruct_from_diff(diff);
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        assert_eq!(before_lines, vec!["line1", "removed1", "removed2", "line2"]);
        assert_eq!(after_lines, vec!["line1", "line2"]);
    }

    #[test]
    fn test_reconstruct_empty_diff_fallback() {
        let diff = "some random text that is not a diff";
        let (before, after) = reconstruct_from_diff(diff);
        assert!(before.is_empty());
        assert_eq!(after, diff);
    }

    #[test]
    fn test_compute_hunks_detects_changes() {
        let before = Rope::from("line1\nline2\nline3\n");
        let after = Rope::from("line1\nmodified\nline3\n");
        let hunks = compute_hunks(&before, &after);
        assert_eq!(hunks.len(), 1);
        // Hunk should cover line index 1 (0-indexed) in both before and after
        assert_eq!(hunks[0].before.start, 1);
        assert_eq!(hunks[0].before.end, 2);
        assert_eq!(hunks[0].after.start, 1);
        assert_eq!(hunks[0].after.end, 2);
    }

    #[test]
    fn test_compute_hunks_no_changes() {
        let content = Rope::from("line1\nline2\nline3\n");
        let hunks = compute_hunks(&content, &content);
        assert!(hunks.is_empty());
    }

    #[test]
    fn test_parse_and_compute_hunks_roundtrip() {
        let diff = "\
@@ -1,3 +1,3 @@
 fn main() {
-    println!(\"hello\");
+    println!(\"hello world\");
 }
";
        let (before, after, hunks) = parse_and_compute_hunks(diff);
        assert!(!hunks.is_empty());
        assert!(before.len_chars() > 0);
        assert!(after.len_chars() > 0);
    }

    #[test]
    fn test_parse_range_spec_with_comma() {
        assert_eq!(parse_range_spec("10,5"), Some((10, 5)));
    }

    #[test]
    fn test_parse_range_spec_without_comma() {
        assert_eq!(parse_range_spec("10"), Some((10, 1)));
    }

    #[test]
    fn test_parse_range_spec_zero_count() {
        assert_eq!(parse_range_spec("10,0"), Some((10, 0)));
    }

    #[test]
    fn test_parse_range_spec_invalid() {
        assert_eq!(parse_range_spec("abc"), None);
    }

    #[test]
    fn test_no_newline_at_end_marker() {
        let diff = "\
@@ -1,2 +1,2 @@
-old line
+new line
 context
\\ No newline at end of file
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        // The "\ No newline" marker should be skipped, not added as a line
        assert_eq!(hunks[0].lines.len(), 3);
    }
}
