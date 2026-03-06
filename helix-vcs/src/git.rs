use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use gix::filter::plumbing::driver::apply::Delay;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use gix::bstr::ByteSlice;
use gix::diff::Rewrites;
use gix::dir::entry::Status;
use gix::objs::tree::EntryKind;
use gix::sec::trust::DefaultForLevel;
use gix::status::{
    index_worktree::Item,
    plumbing::index_as_worktree::{Change, EntryStatus},
    UntrackedFiles,
};
use gix::{Commit, ObjectId, Repository, ThreadSafeRepository};

use crate::{FileChange, StatusEntry};

#[cfg(test)]
mod test;

#[cfg(test)]
mod commit_test;

/// Unquote a path from git porcelain v1 format.
/// Git quotes paths containing special characters (spaces, tabs, newlines, etc.)
/// using C-style escaping with surrounding double quotes.
fn unquote_porcelain_path(s: &str) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        let inner = &s[1..s.len() - 1];
        // Unescape C-style: \n, \t, \r, \", \\
        let mut result = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next) = chars.peek() {
                    match next {
                        'n' => {
                            result.push('\n');
                            chars.next();
                        }
                        't' => {
                            result.push('\t');
                            chars.next();
                        }
                        'r' => {
                            result.push('\r');
                            chars.next();
                        }
                        '"' => {
                            result.push('"');
                            chars.next();
                        }
                        '\\' => {
                            result.push('\\');
                            chars.next();
                        }
                        _ => result.push(c),
                    }
                } else {
                    result.push(c);
                }
            } else {
                result.push(c);
            }
        }
        result
    } else {
        s.to_string()
    }
}

#[inline]
fn get_repo_dir(file: &Path) -> Result<&Path> {
    file.parent().context("file has no parent directory")
}

/// Stage a file in the git index (equivalent to `git add <path>`).
pub fn stage_file(file: &Path) -> Result<()> {
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo_dir = get_repo_dir(&file)?;

    // Use git add to stage the file
    let output = Command::new("git")
        .arg("add")
        .arg(file.as_path())
        .current_dir(repo_dir)
        .output()
        .context("failed to execute git add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git add failed: {}", stderr);
    }

    Ok(())
}

/// Unstage a file from the git index (equivalent to `git reset HEAD <path>`).
pub fn unstage_file(file: &Path) -> Result<()> {
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo_dir = get_repo_dir(&file)?;

    // Use git reset to unstage the file
    let output = Command::new("git")
        .arg("reset")
        .arg("HEAD")
        .arg(file.as_path())
        .current_dir(repo_dir)
        .output()
        .context("failed to execute git reset")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git reset failed: {}", stderr);
    }

    Ok(())
}

pub fn get_diff_base(file: &Path) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    // TODO cache repository lookup

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();
    let head = repo.head_commit()?;
    let file_oid = find_file_in_commit(&repo, &head, &file)?;

    let file_object = repo.find_object(file_oid)?;
    let data = file_object.detach().data;
    // Get the actual data that git would make out of the git object.
    // This will apply the user's git config or attributes like crlf conversions.
    if let Some(work_dir) = repo.workdir() {
        let rela_path = file.strip_prefix(work_dir)?;
        let rela_path = gix::path::try_into_bstr(rela_path)?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let mut worktree_outcome =
            pipeline.convert_to_worktree(&data, rela_path.as_ref(), Delay::Forbid)?;
        let mut buf = Vec::with_capacity(data.len());
        worktree_outcome.read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(data)
    }
}

/// Get the content of a file from the git index (staged version).
/// This retrieves the content that has been staged with `git add`.
pub fn get_index_content(file: &Path) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();

    let work_dir = repo.workdir().context("bare repository has no worktree")?;
    let rela_path = file.strip_prefix(work_dir)?;
    let rela_path_str = rela_path.to_string_lossy();

    // Use git show :path to get staged content from the index
    let output = Command::new("git")
        .args(["show", &format!(":{}", rela_path_str)])
        .current_dir(work_dir)
        .output()
        .context("failed to execute git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git show failed: {}", stderr);
    }

    Ok(output.stdout)
}

/// Revert a hunk by applying a reverse patch using `git apply -R`
pub fn revert_hunk(file_path: &Path, patch: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let file = gix::path::realpath(file_path).context("resolve symlinks")?;
    let repo_dir = get_repo_dir(&file)?;

    // Use git apply -R with stdin to pass the patch
    let mut child = Command::new("git")
        .arg("apply")
        .arg("-R") // Reverse patch
        .arg("--")
        .arg("-")
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git apply -R")?;

    // Write the patch to stdin
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(patch.as_bytes())?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for git apply -R")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git apply -R failed: {}", stderr);
    }

    Ok(())
}

/// Stage a hunk by applying the patch to the index using `git apply --cached`
pub fn stage_hunk(file_path: &Path, patch: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let file = gix::path::realpath(file_path).context("resolve symlinks")?;
    let repo_dir = get_repo_dir(&file)?;

    // Use git apply --cached with stdin to pass the patch
    // --cached applies the patch to the index (staging area) without modifying the worktree
    let mut child = Command::new("git")
        .arg("apply")
        .arg("--cached") // Apply to index (staging area)
        .arg("--")
        .arg("-")
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git apply --cached")?;

    // Write the patch to stdin
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(patch.as_bytes())?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for git apply --cached")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git apply --cached failed: {}", stderr);
    }

    Ok(())
}

/// Unstage a hunk by reverse-applying a patch to the index.
/// Uses `git apply --cached -R` to remove the change from the staging area.
pub fn unstage_hunk(file_path: &Path, patch: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let file = gix::path::realpath(file_path).context("resolve symlinks")?;
    let repo_dir = get_repo_dir(&file)?;

    // Use git apply --cached -R with stdin to reverse-apply the patch
    // --cached applies to the index, -R reverses the patch
    let mut child = Command::new("git")
        .arg("apply")
        .arg("--cached")
        .arg("-R") // Reverse apply
        .arg("--")
        .arg("-")
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git apply --cached -R")?;

    // Write the patch to stdin
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(patch.as_bytes())?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for git apply --cached -R")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git apply --cached -R failed: {}", stderr.trim());
    }

    Ok(())
}

/// Commit staged changes with the given message (equivalent to `git commit -m <message>`).
pub fn commit(cwd: &Path, message: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    // Use git commit with --file=- to read message from stdin
    let mut child = Command::new("git")
        .arg("commit")
        .arg("--file=-") // Read commit message from stdin
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git commit")?;

    // Write the commit message to stdin
    let stdin = child.stdin.as_mut().expect("stdin should be piped");
    stdin.write_all(message.as_bytes())?;

    let output = child
        .wait_with_output()
        .context("failed to wait for git commit")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git commit failed: {}", stderr);
    }

    Ok(())
}

pub fn get_current_head_name(file: &Path) -> Result<Arc<ArcSwap<Box<str>>>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();
    let head_ref = repo.head_ref()?;
    let head_commit = repo.head_commit()?;

    let name = match head_ref {
        Some(reference) => reference.name().shorten().to_string(),
        None => head_commit.id.to_hex_with_len(8).to_string(),
    };

    Ok(Arc::new(ArcSwap::from_pointee(name.into_boxed_str())))
}

pub fn for_each_changed_file(cwd: &Path, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    status(&open_repo(cwd)?.to_thread_local(), f)
}

/// Get git status using porcelain format.
/// Returns a vector of StatusEntry with staged/unstaged info.
///
/// Uses `git status --porcelain=v1` which outputs `XY PATH` format where:
/// - X = staged status (index status)
/// - Y = unstaged status (worktree status)
///
/// A file can appear twice if it has both staged and unstaged changes (e.g., `MM`).
///
/// # Arguments
/// * `cwd` - The working directory to run git status from
/// * `populate_stats` - If true, fetch diff stats (additions/deletions) for each file.
///   Note: This can be slow for many files as it runs `git diff --numstat` for each.
pub fn get_status_porcelain(cwd: &Path, populate_stats: bool) -> Result<Vec<StatusEntry>> {
    let output = Command::new("git")
        .arg("status")
        .arg("--porcelain=v1")
        .current_dir(cwd)
        .output()
        .context("failed to execute git status --porcelain=v1")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git status --porcelain=v1 failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    // Get the repository root to construct absolute paths
    let repo = open_repo(cwd)?;
    let repo_local = repo.to_thread_local();
    let work_dir = repo_local
        .workdir()
        .context("working tree not found")?
        .to_path_buf();

    // Helper to get diff stats for a file if populate_stats is true
    let get_stats = |file_path: &Path, staged: bool| -> (Option<usize>, Option<usize>, bool) {
        if !populate_stats {
            return (None, None, false);
        }
        match get_diff_stats(cwd, file_path, staged) {
            Ok(Some((adds, dels, is_binary))) => (Some(adds), Some(dels), is_binary),
            _ => (None, None, false),
        }
    };

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }

        // Parse the porcelain format: XY PATH or XY OLD_PATH -> NEW_PATH
        // X = index status (staged), Y = worktree status (unstaged)
        if line.len() < 3 {
            continue;
        }

        let x = line.chars().next().unwrap_or(' '); // staged status
        let y = line.chars().nth(1).unwrap_or(' '); // unstaged status
        let path_part = &line[3..]; // Skip "XY "

        // Handle renamed/copied files: "R  OLD -> NEW" or "C  OLD -> NEW" format
        // Note: When Y='R' or Y='C', the staged changes (X status) are at OLD path,
        // not NEW path. E.g., "MR OLD -> NEW" means staged modification at OLD,
        // unstaged rename to NEW.
        let (path, from_path) = if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
            if let Some((old_path, new_path)) = path_part.split_once(" -> ") {
                (
                    unquote_porcelain_path(new_path),
                    Some(unquote_porcelain_path(old_path)),
                )
            } else {
                // Malformed rename entry, skip
                continue;
            }
        } else {
            (unquote_porcelain_path(path_part), None)
        };

        // Check for untracked files first (special case: ??)
        if x == '?' && y == '?' {
            let file_path = work_dir.join(&path);
            let (additions, deletions, is_binary) = get_stats(&file_path, false);
            entries.push(StatusEntry {
                change: FileChange::Untracked { path: file_path },
                staged: false,
                additions,
                deletions,
                is_binary,
            });
            continue;
        }

        // Check for conflict states BEFORE individual status codes
        // Conflicts: UU, AA, DD, AU, UA, DU, UD (all unmerged states)
        let is_conflict = matches!(
            (x, y),
            ('U', 'U')
                | ('A', 'A')
                | ('D', 'D')
                | ('A', 'U')
                | ('U', 'A')
                | ('D', 'U')
                | ('U', 'D')
        );
        if is_conflict {
            let file_path = work_dir.join(&path);
            let (additions, deletions, is_binary) = get_stats(&file_path, true);
            entries.push(StatusEntry {
                change: FileChange::Conflict { path: file_path },
                staged: true,
                additions,
                deletions,
                is_binary,
            });
            continue;
        }

        // Helper to create FileChange from status code
        let make_change = |code: char, path: &str, from_path: Option<&str>| -> Option<FileChange> {
            match code {
                'M' => Some(FileChange::Modified {
                    path: work_dir.join(path),
                }),
                'A' => Some(FileChange::Modified {
                    path: work_dir.join(path),
                }), // Added files treated as modified
                'D' => Some(FileChange::Deleted {
                    path: work_dir.join(path),
                }),
                'R' => Some(FileChange::Renamed {
                    from_path: work_dir.join(from_path.unwrap_or_default()),
                    to_path: work_dir.join(path),
                }),
                'C' => Some(FileChange::Renamed {
                    from_path: work_dir.join(from_path.unwrap_or_default()),
                    to_path: work_dir.join(path),
                }),
                _ => None,
            }
        };

        // Handle staged changes (X status)
        // When Y='R' or Y='C', staged changes are at OLD path (from_path), not NEW path
        if x != ' ' {
            let staged_path = if (y == 'R' || y == 'C') && from_path.is_some() {
                from_path.as_deref().unwrap_or(&path)
            } else {
                &path
            };
            if let Some(change) = make_change(x, staged_path, from_path.as_deref()) {
                let file_path = change.path();
                let (additions, deletions, is_binary) = get_stats(file_path, true);
                entries.push(StatusEntry {
                    change,
                    staged: true,
                    additions,
                    deletions,
                    is_binary,
                });
            }
        }

        // Handle unstaged changes (Y status)
        if y != ' ' {
            if let Some(change) = make_change(y, &path, from_path.as_deref()) {
                let file_path = change.path();
                let (additions, deletions, is_binary) = get_stats(file_path, false);
                entries.push(StatusEntry {
                    change,
                    staged: false,
                    additions,
                    deletions,
                    is_binary,
                });
            }
        }
    }

    Ok(entries)
}

fn open_repo(path: &Path) -> Result<ThreadSafeRepository> {
    // custom open options
    let mut git_open_opts_map = gix::sec::trust::Mapping::<gix::open::Options>::default();

    // On windows various configuration options are bundled as part of the installations
    // This path depends on the install location of git and therefore requires some overhead to lookup
    // This is basically only used on windows and has some overhead hence it's disabled on other platforms.
    // `gitoxide` doesn't use this as default
    let config = gix::open::permissions::Config {
        system: true,
        git: true,
        user: true,
        env: true,
        includes: true,
        git_binary: cfg!(windows),
    };
    // change options for config permissions without touching anything else
    git_open_opts_map.reduced = git_open_opts_map
        .reduced
        .permissions(gix::open::Permissions {
            config,
            ..gix::open::Permissions::default_for_level(gix::sec::Trust::Reduced)
        });
    git_open_opts_map.full = git_open_opts_map.full.permissions(gix::open::Permissions {
        config,
        ..gix::open::Permissions::default_for_level(gix::sec::Trust::Full)
    });

    let open_options = gix::discover::upwards::Options {
        dot_git_only: true,
        ..Default::default()
    };

    let res = ThreadSafeRepository::discover_with_environment_overrides_opts(
        path,
        open_options,
        git_open_opts_map,
    )?;

    Ok(res)
}

/// Emulates the result of running `git status` from the command line.
fn status(repo: &Repository, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working tree not found"))?
        .to_path_buf();

    let status_platform = repo
        .status(gix::progress::Discard)?
        // Here we discard the `status.showUntrackedFiles` config, as it makes little sense in
        // our case to not list new (untracked) files. We could have respected this config
        // if the default value weren't `Collapsed` though, as this default value would render
        // the feature unusable to many.
        .untracked_files(UntrackedFiles::Files)
        // Turn on file rename detection, which is off by default.
        .index_worktree_rewrites(Some(Rewrites {
            copies: None,
            percentage: Some(0.5),
            limit: 1000,
            ..Default::default()
        }));

    // No filtering based on path
    let empty_patterns = vec![];

    let status_iter = status_platform.into_index_worktree_iter(empty_patterns)?;

    for item in status_iter {
        let Ok(item) = item.map_err(|err| f(Err(err.into()))) else {
            continue;
        };
        let change = match item {
            Item::Modification {
                rela_path, status, ..
            } => {
                let path = work_dir.join(rela_path.to_path()?);
                match status {
                    EntryStatus::Conflict { .. } => FileChange::Conflict { path },
                    EntryStatus::Change(Change::Removed) => FileChange::Deleted { path },
                    EntryStatus::Change(Change::Modification { .. }) => {
                        FileChange::Modified { path }
                    }
                    // Files marked with `git add --intent-to-add`. Such files
                    // still show up as new in `git status`, so it's appropriate
                    // to show them the same way as untracked files in the
                    // "changed file" picker. One example of this being used
                    // is Jujutsu, a Git-compatible VCS. It marks all new files
                    // with `--intent-to-add` automatically.
                    EntryStatus::IntentToAdd => FileChange::Untracked { path },
                    _ => continue,
                }
            }
            Item::DirectoryContents { entry, .. } if entry.status == Status::Untracked => {
                FileChange::Untracked {
                    path: work_dir.join(entry.rela_path.to_path()?),
                }
            }
            Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => FileChange::Renamed {
                from_path: work_dir.join(source.rela_path().to_path()?),
                to_path: work_dir.join(dirwalk_entry.rela_path.to_path()?),
            },
            _ => continue,
        };
        if !f(Ok(change)) {
            break;
        }
    }

    Ok(())
}

/// Get the path relative to the repository root for a given file path.
/// Returns the relative path if the file is in a git repository, otherwise returns None.
pub fn get_relative_path(file_path: &Path) -> Option<std::path::PathBuf> {
    // First resolve to absolute path
    let abs_path = match gix::path::realpath(file_path) {
        Ok(p) => p,
        Err(_) => return None,
    };

    // Find the repo root using gix::discover
    // gix::discover::upwards returns (gix::discover::repository::Path, gix::sec::trust::Level)
    let repo_path = match gix::discover::upwards(&abs_path) {
        Ok((path, _)) => path,
        Err(_) => return None,
    };

    // Convert gix::discover::repository::Path to std::path::Path
    // The Path variant contains a PathBuf
    let repo_path_std: &std::path::Path = repo_path.as_ref();

    // Open the repository to get the workdir
    let repo = match open_repo(repo_path_std) {
        Ok(r) => r.to_thread_local(),
        Err(_) => return None,
    };

    // Get the workdir (repo root)
    let repo_root = repo.workdir()?;

    // Strip the repo root to get the relative path
    let rel_path = abs_path.strip_prefix(repo_root).ok()?;

    Some(rel_path.to_path_buf())
}

/// Get diff stats (additions/deletions) for a file.
/// Returns (additions, deletions) or None if binary/unavailable.
///
/// Uses `git diff --numstat` which outputs: `additions\tdeletions\tfilename`
/// - Normal files: parse additions/deletions
/// - Binary files: `- - filename` → returns None
/// - New files: `0 0 filename` → returns Some((0, 0))
///
/// # Arguments
/// * `cwd` - Working directory to run git command in
/// * `file` - File path to get diff stats for
/// * `staged` - If true, get stats for staged changes (HEAD → index).
///              If false, get stats for unstaged changes (index → working directory).
///
/// Returns `Ok(Some((additions, deletions, is_binary)))` where:
/// - `(additions, deletions)` are the line counts (0 for binary files)
/// - `is_binary` is true if the file is detected as binary by git
pub fn get_diff_stats(
    cwd: &Path,
    file: &Path,
    staged: bool,
) -> Result<Option<(usize, usize, bool)>> {
    let mut cmd = Command::new("git");
    cmd.arg("diff").arg("--numstat");

    if staged {
        // Staged: compare HEAD to index (what's staged for commit)
        cmd.arg("--cached").arg("HEAD");
    }
    // Unstaged: compare index to working directory (no HEAD argument needed)

    cmd.arg("--").arg(file);
    cmd.current_dir(cwd);

    let output = cmd
        .output()
        .context("failed to execute git diff --numstat")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff --numstat failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = match stdout.lines().next() {
        Some(l) => l,
        None => return Ok(None), // No output means no changes
    };

    if line.is_empty() {
        return Ok(None);
    }

    // Parse: additions\tdeletions\tfilename
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 2 {
        return Ok(None);
    }

    let additions_str = parts[0];
    let deletions_str = parts[1];

    // Binary files show `- - filename`
    if additions_str == "-" || deletions_str == "-" {
        return Ok(Some((0, 0, true))); // Binary file
    }

    let additions = additions_str
        .parse::<usize>()
        .context("failed to parse additions count")?;
    let deletions = deletions_str
        .parse::<usize>()
        .context("failed to parse deletions count")?;

    Ok(Some((additions, deletions, false))) // Normal file
}

/// Get git log entries for the repository.
/// Returns up to `limit` entries, optionally filtered to a specific file.
pub fn get_log(
    cwd: &Path,
    limit: usize,
    file_path: Option<&Path>,
) -> Result<Vec<crate::status::LogEntry>> {
    // Use NUL byte (%x00) as field separator to avoid ambiguity
    let format = "%H%x00%h%x00%s%x00%an%x00%ad%x00%ar";

    let mut cmd = Command::new("git");
    cmd.arg("log")
        .arg(format!("--format={}", format))
        .arg("--date=short")
        .arg(format!("-n{}", limit))
        .current_dir(cwd);

    // If a file path is provided, filter log to that file
    if let Some(path) = file_path {
        cmd.arg("--").arg(path);
    }

    let output = cmd.output().context("failed to execute git log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git log failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(6, '\0').collect();
        if parts.len() < 6 {
            continue; // Skip malformed lines
        }

        entries.push(crate::status::LogEntry {
            hash: parts[0].to_string(),
            short_hash: parts[1].to_string(),
            subject: parts[2].to_string(),
            author: parts[3].to_string(),
            date: parts[4].to_string(),
            relative_date: parts[5].to_string(),
        });
    }

    Ok(entries)
}

/// Get the stat summary and diff for a specific commit.
/// Returns the output of `git show --stat <hash>` for preview display.
pub fn get_commit_diff(cwd: &Path, hash: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("show")
        .arg("--stat")
        .arg("--format=%H%n%s%n%an <%ae>%n%ad%n")
        .arg("--date=short")
        .arg(hash)
        .current_dir(cwd)
        .output()
        .context("failed to execute git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git show failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get the list of files changed in a specific commit with stats.
/// Returns tuples of (status, file_path, additions, deletions).
pub fn get_commit_files(cwd: &Path, hash: &str) -> Result<Vec<(String, String, usize, usize)>> {
    let output = Command::new("git")
        .arg("diff-tree")
        .arg("--no-commit-id")
        .arg("-r")
        .arg("--numstat")
        .arg(hash)
        .current_dir(cwd)
        .output()
        .context("failed to execute git diff-tree --numstat")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff-tree failed: {}", stderr);
    }

    let numstat_stdout = String::from_utf8_lossy(&output.stdout);

    // Also get name-status for the change type (A/M/D/R)
    let status_output = Command::new("git")
        .arg("diff-tree")
        .arg("--no-commit-id")
        .arg("-r")
        .arg("--name-status")
        .arg(hash)
        .current_dir(cwd)
        .output()
        .context("failed to execute git diff-tree --name-status")?;

    let status_stdout = String::from_utf8_lossy(&status_output.stdout);

    // Parse name-status into a map: path -> status
    let mut status_map = std::collections::HashMap::new();
    for line in status_stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() >= 2 {
            status_map.insert(parts[1].to_string(), parts[0].to_string());
        }
    }

    // Parse numstat
    let mut files = Vec::new();
    for line in numstat_stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            let additions = parts[0].parse::<usize>().unwrap_or(0);
            let deletions = parts[1].parse::<usize>().unwrap_or(0);
            let path = parts[2].to_string();
            let status = status_map
                .get(&path)
                .cloned()
                .unwrap_or_else(|| "M".to_string());
            files.push((status, path, additions, deletions));
        }
    }

    Ok(files)
}

/// Get the content of a file at a specific git revision.
///
/// Runs `git show <revision>:<file_path>` and returns the raw bytes.
/// Returns an empty Vec for revisions where the file doesn't exist (e.g. parent of a newly added file).
pub fn get_file_at_revision(cwd: &Path, revision: &str, file_path: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("show")
        .arg(format!("{}:{}", revision, file_path))
        .current_dir(cwd)
        .output()
        .context("failed to execute git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git show failed: {}", stderr);
    }

    Ok(output.stdout)
}

/// Get the diff for a specific file in a commit.
pub fn get_commit_file_diff(cwd: &Path, hash: &str, file_path: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("diff")
        .arg(format!("{}~1", hash))
        .arg(hash)
        .arg("--")
        .arg(file_path)
        .current_dir(cwd)
        .output()
        .context("failed to execute git diff for commit file")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get blame information for a file.
/// Parses `git blame --porcelain` output into structured BlameLine entries.
pub fn get_blame(file: &Path) -> Result<Vec<crate::status::BlameLine>> {
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo_dir = get_repo_dir(&file)?;

    let repo = open_repo(repo_dir)
        .context("failed to open git repo")?
        .to_thread_local();
    let work_dir = repo.workdir().context("bare repository has no worktree")?;
    let rela_path = file.strip_prefix(work_dir)?;

    let output = Command::new("git")
        .arg("blame")
        .arg("--porcelain")
        .arg(rela_path)
        .current_dir(work_dir)
        .output()
        .context("failed to execute git blame")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git blame failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    // Parse porcelain format: each block starts with a hash line and ends
    // with a tab-prefixed content line.
    let mut current_hash = String::new();
    let mut current_author = String::new();
    let mut current_date = String::new();
    let mut current_line_no: usize = 0;
    let mut is_boundary = false;

    for line in stdout.lines() {
        if line.starts_with('\t') {
            // Content line — ends the current block
            let content = line[1..].to_string();

            entries.push(crate::status::BlameLine {
                hash: current_hash.clone(),
                short_hash: if current_hash.len() >= 7 {
                    current_hash[..7].to_string()
                } else {
                    current_hash.clone()
                },
                author: current_author.clone(),
                date: current_date.clone(),
                relative_date: String::new(),
                line_no: current_line_no,
                content,
                is_boundary,
            });
        } else if let Some(author) = line.strip_prefix("author ") {
            current_author = author.to_string();
        } else if let Some(timestamp_str) = line.strip_prefix("author-time ") {
            if let Ok(timestamp) = timestamp_str.trim().parse::<i64>() {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let diff_secs = now_secs - timestamp;

                current_date = compute_relative_time(diff_secs);
            }
        } else if line == "boundary" {
            is_boundary = true;
        } else if !line.starts_with("author-")
            && !line.starts_with("committer")
            && !line.starts_with("summary ")
            && !line.starts_with("previous ")
            && !line.starts_with("filename ")
            && !line.is_empty()
        {
            // Hash line: "<hash> <orig_line> <final_line> [<num_lines>]"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                current_hash = parts[0].to_string();
                if let Ok(line_no) = parts[2].parse::<usize>() {
                    current_line_no = line_no;
                }
                is_boundary = false;
            }
        }
    }

    // Fill in relative_date from the stored date (already relative)
    for entry in &mut entries {
        entry.relative_date = entry.date.clone();
    }

    Ok(entries)
}

/// List all git stashes.
pub fn stash_list(cwd: &Path) -> Result<Vec<crate::status::StashEntry>> {
    let output = Command::new("git")
        .arg("stash")
        .arg("list")
        .arg("--format=%gd%x00%s%x00%cr")
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash list failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, '\0').collect();
        if parts.len() < 3 {
            continue;
        }

        entries.push(crate::status::StashEntry {
            index: parts[0].to_string(),
            message: parts[1].to_string(),
            relative_date: parts[2].to_string(),
        });
    }

    Ok(entries)
}

/// Show the diff for a specific stash entry.
pub fn stash_show(cwd: &Path, index: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("stash")
        .arg("show")
        .arg("-p")
        .arg(index)
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash show failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Push current changes to a new stash entry.
pub fn stash_push(cwd: &Path, message: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("stash").arg("push").current_dir(cwd);

    if let Some(msg) = message {
        cmd.arg("-m").arg(msg);
    }

    let output = cmd.output().context("failed to execute git stash push")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash push failed: {}", stderr);
    }

    Ok(())
}

/// Pop a stash entry (apply and remove).
pub fn stash_pop(cwd: &Path, index: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("stash")
        .arg("pop")
        .arg(index)
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash pop")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash pop failed: {}", stderr);
    }

    Ok(())
}

/// Apply a stash entry without removing it.
pub fn stash_apply(cwd: &Path, index: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("stash")
        .arg("apply")
        .arg(index)
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash apply")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash apply failed: {}", stderr);
    }

    Ok(())
}

/// Drop (delete) a stash entry.
pub fn stash_drop(cwd: &Path, index: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("stash")
        .arg("drop")
        .arg(index)
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash drop")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash drop failed: {}", stderr);
    }

    Ok(())
}

/// Get the list of files changed in a stash entry with line stats.
/// Returns tuples of (status, path, additions, deletions).
pub fn get_stash_files(cwd: &Path, index: &str) -> Result<Vec<(String, String, usize, usize)>> {
    let output = Command::new("git")
        .arg("stash")
        .arg("show")
        .arg("--numstat")
        .arg(index)
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash show --numstat")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash show failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            let additions = parts[0].parse::<usize>().unwrap_or(0);
            let deletions = parts[1].parse::<usize>().unwrap_or(0);
            let path = parts[2].to_string();
            // stash show --numstat doesn't provide status; assume modified
            let status = "M".to_string();
            files.push((status, path, additions, deletions));
        }
    }

    Ok(files)
}

/// Get the diff for a specific file in a stash entry.
pub fn get_stash_file_diff(cwd: &Path, index: &str, file_path: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("stash")
        .arg("show")
        .arg("-p")
        .arg(index)
        .arg("--")
        .arg(file_path)
        .current_dir(cwd)
        .output()
        .context("failed to execute git stash show -p")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash show failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Convert a duration in seconds to a human-readable relative time string.
fn compute_relative_time(diff_secs: i64) -> String {
    if diff_secs < 60 {
        "just now".to_string()
    } else if diff_secs < 3600 {
        format!("{} min ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{} hours ago", diff_secs / 3600)
    } else if diff_secs < 604_800 {
        format!("{} days ago", diff_secs / 86400)
    } else if diff_secs < 2_592_000 {
        format!("{} weeks ago", diff_secs / 604_800)
    } else if diff_secs < 31_536_000 {
        format!("{} months ago", diff_secs / 2_592_000)
    } else {
        format!("{} years ago", diff_secs / 31_536_000)
    }
}

/// Finds the object that contains the contents of a file at a specific commit.
fn find_file_in_commit(repo: &Repository, commit: &Commit, file: &Path) -> Result<ObjectId> {
    let repo_dir = repo.workdir().context("repo has no worktree")?;
    let rel_path = file.strip_prefix(repo_dir)?;
    let tree = commit.tree()?;
    let tree_entry = tree
        .lookup_entry_by_path(rel_path)?
        .context("file is untracked")?;
    match tree_entry.mode().kind() {
        // not a file, everything is new, do not show diff
        mode @ (EntryKind::Tree | EntryKind::Commit | EntryKind::Link) => {
            bail!("entry at {} is not a file but a {mode:?}", file.display())
        }
        // found a file
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(tree_entry.object_id()),
    }
}
