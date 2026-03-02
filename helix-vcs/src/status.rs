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
}
