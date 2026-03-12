// Permission review diff view — shows proposed agent edits for approval/rejection.
//
// Provides unified diff parsing and before/after content reconstruction so that
// agent-proposed changes can be displayed using the same DiffView infrastructure.

use helix_core::Rope;
use helix_vcs::Hunk;
use imara_diff::{Algorithm, Diff, InternedInput};

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
