use crate::compositor::{Callback, Component, Compositor, Context, Event, EventResult};
use helix_vcs::BlameLine;
use helix_view::graphics::{Color, Rect, Style};

use tui::buffer::Buffer as Surface;
use tui::text::Span;
use tui::widgets::{Block, Widget};

// =============================================================================
// Blame Annotation Layout Constants
// =============================================================================

/// Fixed width for the blame annotation column (hash + author + date).
const ANNOTATION_WIDTH: u16 = 35;

/// Width for the line number column (e.g., " 1234 ").
const LINE_NUM_WIDTH: u16 = 5;

/// Width for a single separator character ("│").
const SEPARATOR_WIDTH: u16 = 1;

/// Maximum display length for author names.
const MAX_AUTHOR_LEN: usize = 10;

/// Maximum display length for relative dates.
const MAX_DATE_LEN: usize = 8;

// =============================================================================
// BlameView Component
// =============================================================================

/// Full-screen blame view showing file content with per-line blame annotations.
///
/// Layout per row:
/// ```text
/// │ a1b2c3d Alice    2h ago │  1 │ fn main() {
/// │ a1b2c3d Alice    2h ago │  2 │     let x = 42;
/// │         (continuation)  │  3 │     println!("{}", x);
/// │ f4e5d6c Bob     3d ago  │  4 │ }
/// ```
///
/// Consecutive lines from the same commit are grouped: only the first line
/// in a group shows the annotation; continuation lines show a dimmed separator.
pub struct BlameView {
    /// Blame data for each line (1:1 with file lines).
    blame_lines: Vec<BlameLine>,
    /// File name shown in the header.
    file_name: String,
    /// Current scroll offset (0-indexed top-of-viewport line).
    scroll: usize,
    /// Currently selected line (0-indexed).
    selected_line: usize,
    /// Visible line count from the last render pass.
    visible_lines: usize,
}

impl BlameView {
    pub fn new(blame_lines: Vec<BlameLine>, file_name: String, cursor_line: usize) -> Self {
        let selected_line = cursor_line.min(blame_lines.len().saturating_sub(1));
        let scroll = if selected_line > 10 {
            selected_line - 10
        } else {
            0
        };
        Self {
            blame_lines,
            file_name,
            scroll,
            selected_line,
            visible_lines: 20,
        }
    }

    // -------------------------------------------------------------------------
    // Annotation formatting
    // -------------------------------------------------------------------------

    /// Format a blame annotation for a single line.
    ///
    /// For the first line in a commit group the annotation shows
    /// `<short_hash> <author> <relative_date>`. Continuation lines within the
    /// same group show only a dimmed separator to reduce visual noise.
    fn format_annotation(line: &BlameLine, is_first_in_group: bool) -> String {
        if !is_first_in_group {
            // Continuation line — right-aligned separator only.
            return format!("{:>width$}", "│", width = ANNOTATION_WIDTH as usize);
        }

        let author = truncate_str(&line.author, MAX_AUTHOR_LEN);
        let date = truncate_str(&line.relative_date, MAX_DATE_LEN);

        format!("{} {:>10} {:>8}", &line.short_hash, author, date)
    }

    /// Returns `true` when `index` is the first line of a new blame group
    /// (i.e. a different commit than the previous line).
    fn is_first_in_group(&self, index: usize) -> bool {
        if index == 0 {
            return true;
        }
        self.blame_lines[index].hash != self.blame_lines[index - 1].hash
    }

    /// Derive a deterministic muted colour from a commit hash.
    ///
    /// The first six bytes of the hash string are mapped into the RGB range
    /// 80–179 so the resulting colour is neither too bright nor too dark.
    fn hash_color(hash: &str) -> Color {
        let bytes = hash.as_bytes();
        if bytes.len() < 6 {
            return Color::Gray;
        }
        let r = 80 + (bytes[0] % 100);
        let g = 80 + (bytes[2] % 100);
        let b = 80 + (bytes[4] % 100);
        Color::Rgb(r, g, b)
    }

    // -------------------------------------------------------------------------
    // Scroll / selection helpers
    // -------------------------------------------------------------------------

    fn scroll_up(&mut self, count: usize) {
        self.selected_line = self.selected_line.saturating_sub(count);
        if self.selected_line < self.scroll {
            self.scroll = self.selected_line;
        }
    }

    fn scroll_down(&mut self, count: usize) {
        let max_line = self.blame_lines.len().saturating_sub(1);
        self.selected_line = (self.selected_line + count).min(max_line);
        if self.selected_line >= self.scroll + self.visible_lines {
            self.scroll = self.selected_line.saturating_sub(self.visible_lines) + 1;
        }
    }

    fn jump_to_start(&mut self) {
        self.selected_line = 0;
        self.scroll = 0;
    }

    fn jump_to_end(&mut self) {
        self.selected_line = self.blame_lines.len().saturating_sub(1);
        if self.selected_line >= self.visible_lines {
            self.scroll = self.selected_line - self.visible_lines + 1;
        }
    }
}

// =============================================================================
// Component implementation
// =============================================================================

impl Component for BlameView {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        if area.width < 20 || area.height < 3 {
            return;
        }

        // -- Header / border ------------------------------------------------
        let header_text = format!(
            " Blame: {} ({} lines) ",
            self.file_name,
            self.blame_lines.len()
        );
        let header_style = cx.editor.theme.get("ui.statusline");
        let block = Block::bordered()
            .title(Span::styled(header_text, header_style))
            .border_style(header_style);
        let inner = block.inner(area);
        block.render(area, surface);

        if inner.width < 10 || inner.height < 1 {
            return;
        }

        // -- Layout geometry -------------------------------------------------
        self.visible_lines = inner.height as usize;

        let content_start =
            inner.x + ANNOTATION_WIDTH + SEPARATOR_WIDTH + LINE_NUM_WIDTH + SEPARATOR_WIDTH;
        let content_width = inner
            .width
            .saturating_sub(ANNOTATION_WIDTH + SEPARATOR_WIDTH + LINE_NUM_WIDTH + SEPARATOR_WIDTH);

        // -- Theme styles ----------------------------------------------------
        let style_selected = cx.editor.theme.get("ui.cursor.primary");
        let style_line_nr = cx.editor.theme.get("ui.linenr");
        let style_separator = Style::default().fg(Color::Gray);

        // -- Render each visible row -----------------------------------------
        for row in 0..inner.height {
            let line_idx = self.scroll + row as usize;
            if line_idx >= self.blame_lines.len() {
                break;
            }

            let blame_line = &self.blame_lines[line_idx];
            let is_selected = line_idx == self.selected_line;
            let is_first = self.is_first_in_group(line_idx);
            let hash_color = Self::hash_color(&blame_line.hash);
            let y = inner.y + row;

            // -- Annotation column -------------------------------------------
            let annotation = Self::format_annotation(blame_line, is_first);
            let ann_style = if is_first {
                Style::default().fg(hash_color)
            } else {
                Style::default().fg(Color::Gray)
            };
            let ann_style = if is_selected {
                ann_style.patch(style_selected)
            } else {
                ann_style
            };

            let ann_display = fit_to_width(&annotation, ANNOTATION_WIDTH as usize);
            surface.set_stringn(
                inner.x,
                y,
                &ann_display,
                ANNOTATION_WIDTH as usize,
                ann_style,
            );

            // -- First separator ("│") ----------------------------------------
            surface.set_stringn(inner.x + ANNOTATION_WIDTH, y, "│", 1, style_separator);

            // -- Line number --------------------------------------------------
            let line_nr = format!("{:>4} ", blame_line.line_no);
            let nr_style = if is_selected {
                style_line_nr.patch(style_selected)
            } else {
                style_line_nr
            };
            surface.set_stringn(
                inner.x + ANNOTATION_WIDTH + SEPARATOR_WIDTH,
                y,
                &line_nr,
                LINE_NUM_WIDTH as usize,
                nr_style,
            );

            // -- Second separator ("│") ---------------------------------------
            surface.set_stringn(
                inner.x + ANNOTATION_WIDTH + SEPARATOR_WIDTH + LINE_NUM_WIDTH,
                y,
                "│",
                1,
                style_separator,
            );

            // -- File content -------------------------------------------------
            let content_style = if is_selected {
                Style::default().patch(style_selected)
            } else {
                Style::default()
            };
            surface.set_stringn(
                content_start,
                y,
                &blame_line.content,
                content_width as usize,
                content_style,
            );
        }
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        use helix_view::keyboard::KeyCode;

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                let close: Callback = Box::new(|compositor: &mut Compositor, _cx: &mut Context| {
                    compositor.pop();
                });
                EventResult::Consumed(Some(close))
            }

            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up(1);
                EventResult::Consumed(None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down(1);
                EventResult::Consumed(None)
            }

            KeyCode::PageUp => {
                self.scroll_up(self.visible_lines);
                EventResult::Consumed(None)
            }
            KeyCode::PageDown => {
                self.scroll_down(self.visible_lines);
                EventResult::Consumed(None)
            }

            KeyCode::Home | KeyCode::Char('g') => {
                self.jump_to_start();
                EventResult::Consumed(None)
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.jump_to_end();
                EventResult::Consumed(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
}

// =============================================================================
// String helpers
// =============================================================================

/// Truncate `s` to at most `max_len` characters (byte-safe via `char_indices`).
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    match s.char_indices().nth(max_len) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Pad or truncate `s` to exactly `width` characters (left-aligned).
fn fit_to_width(s: &str, width: usize) -> String {
    if s.len() > width {
        s[..width].to_string()
    } else {
        format!("{:<width$}", s, width = width)
    }
}
