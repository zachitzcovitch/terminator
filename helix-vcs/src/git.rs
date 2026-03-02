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
pub fn get_status_porcelain(cwd: &Path) -> Result<Vec<StatusEntry>> {
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
            entries.push(StatusEntry {
                change: FileChange::Untracked {
                    path: work_dir.join(&path),
                },
                staged: false,
                additions: None,
                deletions: None,
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
            entries.push(StatusEntry {
                change: FileChange::Conflict {
                    path: work_dir.join(&path),
                },
                staged: true,
                additions: None,
                deletions: None,
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
                entries.push(StatusEntry {
                    change,
                    staged: true,
                    additions: None,
                    deletions: None,
                });
            }
        }

        // Handle unstaged changes (Y status)
        if y != ' ' {
            if let Some(change) = make_change(y, &path, from_path.as_deref()) {
                entries.push(StatusEntry {
                    change,
                    staged: false,
                    additions: None,
                    deletions: None,
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
