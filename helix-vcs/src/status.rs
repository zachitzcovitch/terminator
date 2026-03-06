use std::path::{Path, PathBuf};

/// States for a file having been changed.
#[derive(Clone)]
pub enum FileChange {
    /// Not tracked by the VCS.
    Untracked { path: PathBuf },
    /// File has been modified.
    Modified { path: PathBuf },
    /// File modification is in conflict with a different update.
    Conflict { path: PathBuf },
    /// File has been deleted.
    Deleted { path: PathBuf },
    /// File has been renamed.
    Renamed {
        from_path: PathBuf,
        to_path: PathBuf,
    },
}

impl FileChange {
    pub fn path(&self) -> &Path {
        match self {
            Self::Untracked { path } => path,
            Self::Modified { path } => path,
            Self::Conflict { path } => path,
            Self::Deleted { path } => path,
            Self::Renamed { to_path, .. } => to_path,
        }
    }
}

/// Represents a file change with staged/unstaged status information.
/// Used for porcelain-style git status output.
#[derive(Clone)]
pub struct StatusEntry {
    /// The file change (path and type of change)
    pub change: FileChange,
    /// Whether this change is staged (in the index)
    pub staged: bool,
    /// Number of additions (from git diff --numstat), if available
    pub additions: Option<usize>,
    /// Number of deletions (from git diff --numstat), if available
    pub deletions: Option<usize>,
    /// Whether this file is binary (detected by git)
    pub is_binary: bool,
}

/// Represents a single git log entry (commit).
#[derive(Clone)]
pub struct LogEntry {
    /// Full commit hash (40 chars)
    pub hash: String,
    /// Short commit hash (7 chars)
    pub short_hash: String,
    /// Commit subject (first line of message)
    pub subject: String,
    /// Author name
    pub author: String,
    /// Date in short format (YYYY-MM-DD)
    pub date: String,
    /// Relative date (e.g., "2 hours ago")
    pub relative_date: String,
}

/// Represents a single line of git blame output.
#[derive(Clone, Debug)]
pub struct BlameLine {
    /// Full commit hash
    pub hash: String,
    /// Short commit hash (7 chars)
    pub short_hash: String,
    /// Author name
    pub author: String,
    /// Date in short format (YYYY-MM-DD)
    pub date: String,
    /// Relative date (e.g., "2 hours ago")
    pub relative_date: String,
    /// Line number in the current file (1-indexed)
    pub line_no: usize,
    /// Line content
    pub content: String,
    /// Whether this is a boundary commit (root of the file's history)
    pub is_boundary: bool,
}
