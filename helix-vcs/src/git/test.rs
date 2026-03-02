use std::{fs::File, io::Write, path::Path, process::Command};

use tempfile::TempDir;

use crate::git;

fn exec_git_cmd(args: &str, git_dir: &Path) {
    let res = Command::new("git")
        .arg("-C")
        .arg(git_dir) // execute the git command in this directory
        .args(args.split_whitespace())
        .env_remove("GIT_DIR")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env("GIT_TERMINAL_PROMPT", "false")
        .env("GIT_AUTHOR_DATE", "2000-01-01 00:00:00 +0000")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_NAME", "author")
        .env("GIT_COMMITTER_DATE", "2000-01-02 00:00:00 +0000")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_NAME", "committer")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_1", "main")
        .output()
        .unwrap_or_else(|_| panic!("`git {args}` failed"));
    if !res.status.success() {
        println!("{}", String::from_utf8_lossy(&res.stdout));
        eprintln!("{}", String::from_utf8_lossy(&res.stderr));
        panic!("`git {args}` failed (see output above)")
    }
}

fn create_commit(repo: &Path, add_modified: bool) {
    if add_modified {
        exec_git_cmd("add -A", repo);
    }
    exec_git_cmd("commit -m message", repo);
}

fn empty_git_repo() -> TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir for git testing");
    exec_git_cmd("init", tmp.path());
    exec_git_cmd("config user.email test@helix.org", tmp.path());
    exec_git_cmd("config user.name helix-test", tmp.path());
    tmp
}

#[test]
fn missing_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"foo").unwrap();

    assert!(git::get_diff_base(&file).is_err());
}

#[test]
fn unmodified_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = b"foo".as_slice();
    File::create(&file).unwrap().write_all(contents).unwrap();
    create_commit(temp_git.path(), true);
    assert_eq!(git::get_diff_base(&file).unwrap(), Vec::from(contents));
}

#[test]
fn modified_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = b"foo".as_slice();
    File::create(&file).unwrap().write_all(contents).unwrap();
    create_commit(temp_git.path(), true);
    File::create(&file).unwrap().write_all(b"bar").unwrap();

    assert_eq!(git::get_diff_base(&file).unwrap(), Vec::from(contents));
}

/// Test that `get_file_head` does not return content for a directory.
/// This is important to correctly cover cases where a directory is removed and replaced by a file.
/// If the contents of the directory object were returned a diff between a path and the directory children would be produced.
#[test]
fn directory() {
    let temp_git = empty_git_repo();
    let dir = temp_git.path().join("file.txt");
    std::fs::create_dir(&dir).expect("");
    let file = dir.join("file.txt");
    let contents = b"foo".as_slice();
    File::create(file).unwrap().write_all(contents).unwrap();

    create_commit(temp_git.path(), true);

    std::fs::remove_dir_all(&dir).unwrap();
    File::create(&dir).unwrap().write_all(b"bar").unwrap();
    assert!(git::get_diff_base(&dir).is_err());
}

/// Test that `get_diff_base` resolves symlinks so that the same diff base is
/// used as the target file.
///
/// This is important to correctly cover cases where a symlink is removed and
/// replaced by a file. If the contents of the symlink object were returned
/// a diff between a literal file path and the actual file content would be
/// produced (bad ui).
#[cfg(any(unix, windows))]
#[test]
fn symlink() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(not(unix))]
    use std::os::windows::fs::symlink_file as symlink;

    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = Vec::from(b"foo");
    File::create(&file).unwrap().write_all(&contents).unwrap();
    let file_link = temp_git.path().join("file_link.txt");

    symlink("file.txt", &file_link).unwrap();
    create_commit(temp_git.path(), true);

    assert_eq!(git::get_diff_base(&file_link).unwrap(), contents);
    assert_eq!(git::get_diff_base(&file).unwrap(), contents);
}

/// Test that `get_diff_base` returns content when the file is a symlink to
/// another file that is in a git repo, but the symlink itself is not.
#[cfg(any(unix, windows))]
#[test]
fn symlink_to_git_repo() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(not(unix))]
    use std::os::windows::fs::symlink_file as symlink;

    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let temp_git = empty_git_repo();

    let file = temp_git.path().join("file.txt");
    let contents = Vec::from(b"foo");
    File::create(&file).unwrap().write_all(&contents).unwrap();
    create_commit(temp_git.path(), true);

    let file_link = temp_dir.path().join("file_link.txt");
    symlink(&file, &file_link).unwrap();

    assert_eq!(git::get_diff_base(&file_link).unwrap(), contents);
    assert_eq!(git::get_diff_base(&file).unwrap(), contents);
}

// ============================================================================
// Tests for get_status_porcelain
// ============================================================================

/// Helper to find a StatusEntry by path suffix
fn find_entry_by_path<'a>(
    entries: &'a [super::StatusEntry],
    path_suffix: &str,
) -> Option<&'a super::StatusEntry> {
    entries
        .iter()
        .find(|e| e.change.path().to_string_lossy().ends_with(path_suffix))
}

/// Test 1: Staged modification (`M  file.txt`) → staged=true, Modified
#[test]
fn status_porcelain_staged_modification() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"original").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage the file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add file.txt", temp_git.path());

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    assert_eq!(entries.len(), 1, "Should have exactly one entry");
    let entry = &entries[0];
    assert!(entry.staged, "Entry should be staged");
    assert!(
        matches!(entry.change, crate::FileChange::Modified { .. }),
        "Entry should be Modified variant"
    );
    assert!(entry.change.path().ends_with("file.txt"));
}

/// Test 2: Unstaged modification (` M file.txt`) → staged=false, Modified
#[test]
fn status_porcelain_unstaged_modification() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"original").unwrap();
    create_commit(temp_git.path(), true);

    // Modify without staging
    File::create(&file).unwrap().write_all(b"modified").unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    assert_eq!(entries.len(), 1, "Should have exactly one entry");
    let entry = &entries[0];
    assert!(!entry.staged, "Entry should be unstaged");
    assert!(
        matches!(entry.change, crate::FileChange::Modified { .. }),
        "Entry should be Modified variant"
    );
}

/// Test 3: Dual staged/unstaged (`MM file.txt`) → TWO entries
#[test]
fn status_porcelain_dual_staged_unstaged() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"original").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage
    File::create(&file).unwrap().write_all(b"staged").unwrap();
    exec_git_cmd("add file.txt", temp_git.path());

    // Modify again without staging
    File::create(&file).unwrap().write_all(b"unstaged").unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    assert_eq!(entries.len(), 2, "Should have two entries for MM state");

    let staged_count = entries.iter().filter(|e| e.staged).count();
    let unstaged_count = entries.iter().filter(|e| !e.staged).count();
    assert_eq!(staged_count, 1, "Should have one staged entry");
    assert_eq!(unstaged_count, 1, "Should have one unstaged entry");

    // Both should be Modified
    for entry in &entries {
        assert!(
            matches!(entry.change, crate::FileChange::Modified { .. }),
            "Both entries should be Modified variant"
        );
    }
}

/// Test 4: Staged new file (`A  file.txt`) → staged=true, Modified (added treated as modified)
#[test]
fn status_porcelain_staged_new_file() {
    let temp_git = empty_git_repo();

    // Create initial commit so we have a valid HEAD
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create and stage a new file
    let new_file = temp_git.path().join("new_file.txt");
    File::create(&new_file)
        .unwrap()
        .write_all(b"new content")
        .unwrap();
    exec_git_cmd("add new_file.txt", temp_git.path());

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = find_entry_by_path(&entries, "new_file.txt").expect("Should find new_file.txt");
    assert!(entry.staged, "New file should be staged");
    assert!(
        matches!(entry.change, crate::FileChange::Modified { .. }),
        "Added file should be treated as Modified"
    );
}

/// Test 5: Untracked file (`?? file.txt`) → staged=false, Untracked
#[test]
fn status_porcelain_untracked_file() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create an untracked file
    let untracked = temp_git.path().join("untracked.txt");
    File::create(&untracked)
        .unwrap()
        .write_all(b"untracked")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = find_entry_by_path(&entries, "untracked.txt").expect("Should find untracked.txt");
    assert!(!entry.staged, "Untracked file should not be staged");
    assert!(
        matches!(entry.change, crate::FileChange::Untracked { .. }),
        "Entry should be Untracked variant"
    );
}

/// Test 6: Deleted file (`D  file.txt`) → staged=true, Deleted
#[test]
fn status_porcelain_staged_deleted_file() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("to_delete.txt");
    File::create(&file).unwrap().write_all(b"content").unwrap();
    create_commit(temp_git.path(), true);

    // Delete and stage the deletion
    std::fs::remove_file(&file).unwrap();
    exec_git_cmd("add to_delete.txt", temp_git.path());

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    assert_eq!(entries.len(), 1, "Should have exactly one entry");
    let entry = &entries[0];
    assert!(entry.staged, "Deleted file should be staged");
    assert!(
        matches!(entry.change, crate::FileChange::Deleted { .. }),
        "Entry should be Deleted variant"
    );
}

/// Test 7: Renamed file (`R  old -> new`) → staged=true, Renamed
#[test]
fn status_porcelain_renamed_file() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let old_file = temp_git.path().join("old_name.txt");
    File::create(&old_file)
        .unwrap()
        .write_all(b"content")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Rename the file and stage it
    let new_file = temp_git.path().join("new_name.txt");
    std::fs::rename(&old_file, &new_file).unwrap();
    exec_git_cmd("add -A", temp_git.path());

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = entries
        .iter()
        .find(|e| matches!(e.change, crate::FileChange::Renamed { .. }))
        .expect("Should have a Renamed entry");

    assert!(entry.staged, "Renamed file should be staged");

    if let crate::FileChange::Renamed { from_path, to_path } = &entry.change {
        assert!(
            from_path.ends_with("old_name.txt"),
            "from_path should be old_name.txt"
        );
        assert!(
            to_path.ends_with("new_name.txt"),
            "to_path should be new_name.txt"
        );
    } else {
        panic!("Expected Renamed variant");
    }
}

/// Test 8: Conflict states (`UU`, `AA`, `DD`, `AU`, `UA`, `DU`, `UD`) → Conflict
#[test]
fn status_porcelain_conflict_uu() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("conflict.txt");
    File::create(&file).unwrap().write_all(b"base\n").unwrap();
    create_commit(temp_git.path(), true);

    // Create a branch and modify the file
    exec_git_cmd("checkout -b feature", temp_git.path());
    File::create(&file)
        .unwrap()
        .write_all(b"feature\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Go back to main and make conflicting change
    exec_git_cmd("checkout main", temp_git.path());
    File::create(&file).unwrap().write_all(b"main\n").unwrap();
    create_commit(temp_git.path(), true);

    // Merge to create conflict
    let result = Command::new("git")
        .args(["-C", temp_git.path().to_str().unwrap(), "merge", "feature"])
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .output()
        .unwrap();
    // Merge should fail due to conflict
    assert!(
        !result.status.success(),
        "Merge should fail due to conflict"
    );

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = find_entry_by_path(&entries, "conflict.txt").expect("Should find conflict.txt");
    assert!(
        matches!(entry.change, crate::FileChange::Conflict { .. }),
        "Entry should be Conflict variant for UU state"
    );
}

/// Test 9: Quoted path (`"file with spaces.txt"`) → unquoted path
#[test]
fn status_porcelain_quoted_path_spaces() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with spaces in the name
    let spaced_file = temp_git.path().join("file with spaces.txt");
    File::create(&spaced_file)
        .unwrap()
        .write_all(b"content")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = entries
        .iter()
        .find(|e| {
            e.change
                .path()
                .to_string_lossy()
                .contains("file with spaces.txt")
        })
        .expect("Should find file with spaces");

    assert!(
        matches!(entry.change, crate::FileChange::Untracked { .. }),
        "Entry should be Untracked"
    );
    assert!(!entry.staged);
}

/// Test 10: Escaped characters (`"file\ttab"`) → tab character
#[test]
fn status_porcelain_escaped_tab() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with a tab character in the name
    let tab_file = temp_git.path().join("file\ttab.txt");
    File::create(&tab_file)
        .unwrap()
        .write_all(b"content")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    // The path should contain an actual tab character, not \t
    let entry = entries
        .iter()
        .find(|e| e.change.path().to_string_lossy().contains('\t'))
        .expect("Should find file with tab in name");

    assert!(
        matches!(entry.change, crate::FileChange::Untracked { .. }),
        "Entry should be Untracked"
    );

    // Verify the path contains actual tab, not escaped \t
    let path_str = entry.change.path().to_string_lossy();
    assert!(
        path_str.contains('\t'),
        "Path should contain actual tab character"
    );
    assert!(
        !path_str.contains("\\t"),
        "Path should not contain escaped \\t"
    );
}

/// Test unquote_porcelain_path function directly
#[test]
fn unquote_porcelain_path_basic() {
    // Test unquoted path
    assert_eq!(git::unquote_porcelain_path("simple.txt"), "simple.txt");

    // Test quoted path with spaces
    assert_eq!(
        git::unquote_porcelain_path("\"file with spaces.txt\""),
        "file with spaces.txt"
    );

    // Test escaped characters
    assert_eq!(
        git::unquote_porcelain_path("\"file\\ttab.txt\""),
        "file\ttab.txt"
    );
    assert_eq!(
        git::unquote_porcelain_path("\"file\\nnewline.txt\""),
        "file\nnewline.txt"
    );
    assert_eq!(
        git::unquote_porcelain_path("\"file\\rcarriage.txt\""),
        "file\rcarriage.txt"
    );
    assert_eq!(
        git::unquote_porcelain_path("\"file\\\"quote.txt\""),
        "file\"quote.txt"
    );
    assert_eq!(
        git::unquote_porcelain_path("\"file\\\\backslash.txt\""),
        "file\\backslash.txt"
    );
}

/// Test empty repository returns empty vec
#[test]
fn status_porcelain_empty_repo() {
    let temp_git = empty_git_repo();

    // Create initial commit (empty repo with no files)
    exec_git_cmd("commit --allow-empty -m 'initial'", temp_git.path());

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();
    assert!(entries.is_empty(), "Empty repo should have no entries");
}

/// Test multiple files with different states
#[test]
fn status_porcelain_multiple_files() {
    let temp_git = empty_git_repo();

    // Create and commit initial files
    let file1 = temp_git.path().join("file1.txt");
    let file2 = temp_git.path().join("file2.txt");
    File::create(&file1).unwrap().write_all(b"file1").unwrap();
    File::create(&file2).unwrap().write_all(b"file2").unwrap();
    create_commit(temp_git.path(), true);

    // Create different states
    // file1: staged modification
    File::create(&file1)
        .unwrap()
        .write_all(b"modified1")
        .unwrap();
    exec_git_cmd("add file1.txt", temp_git.path());

    // file2: unstaged modification
    File::create(&file2)
        .unwrap()
        .write_all(b"modified2")
        .unwrap();

    // file3: untracked
    let file3 = temp_git.path().join("file3.txt");
    File::create(&file3)
        .unwrap()
        .write_all(b"untracked")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    assert_eq!(entries.len(), 3, "Should have three entries");

    // Verify each entry
    let staged_modified = find_entry_by_path(&entries, "file1.txt").expect("Should find file1");
    assert!(staged_modified.staged);
    assert!(matches!(
        staged_modified.change,
        crate::FileChange::Modified { .. }
    ));

    let unstaged_modified = find_entry_by_path(&entries, "file2.txt").expect("Should find file2");
    assert!(!unstaged_modified.staged);
    assert!(matches!(
        unstaged_modified.change,
        crate::FileChange::Modified { .. }
    ));

    let untracked = find_entry_by_path(&entries, "file3.txt").expect("Should find file3");
    assert!(!untracked.staged);
    assert!(matches!(
        untracked.change,
        crate::FileChange::Untracked { .. }
    ));
}

// ============================================================================
// ADVERSARIAL SECURITY TESTS for get_status_porcelain and unquote_porcelain_path
// ============================================================================

// ---------------------------------------------------------------------------
// ATTACK VECTOR 1: Malformed porcelain output (missing status codes, truncated lines)
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with truncated quoted string (missing closing quote)
#[test]
fn adversarial_unquote_truncated_quote() {
    // Only opening quote, no closing quote - should return as-is
    assert_eq!(git::unquote_porcelain_path("\"incomplete"), "\"incomplete");
}

/// Test unquote_porcelain_path with empty quoted string
#[test]
fn adversarial_unquote_empty_quoted() {
    // Empty quoted string "" should return empty string
    assert_eq!(git::unquote_porcelain_path("\"\""), "");
}

/// Test unquote_porcelain_path with single character quoted
#[test]
fn adversarial_unquote_single_char_quoted() {
    assert_eq!(git::unquote_porcelain_path("\"a\""), "a");
}

/// Test unquote_porcelain_path with lone backslash at end (incomplete quote)
#[test]
fn adversarial_unquote_trailing_backslash() {
    // Trailing backslash in incomplete quoted string - returns as-is
    // Function requires both opening AND closing quotes to process
    assert_eq!(git::unquote_porcelain_path("\"trailing\\"), "\"trailing\\");
}

/// Test unquote_porcelain_path with incomplete escape sequence (incomplete quote)
#[test]
fn adversarial_unquote_incomplete_escape() {
    // Backslash at end of string with incomplete quote - returns as-is
    // Function requires both opening AND closing quotes to process
    assert_eq!(git::unquote_porcelain_path("\"end\\"), "\"end\\");
}

/// Test unquote_porcelain_path with trailing backslash in properly quoted string
#[test]
fn adversarial_unquote_trailing_backslash_quoted() {
    // Trailing backslash in properly quoted string - backslash preserved
    assert_eq!(git::unquote_porcelain_path("\"trailing\\\""), "trailing\\");
}

/// Test unquote_porcelain_path with escape at end of properly quoted string
#[test]
fn adversarial_unquote_escape_at_end_quoted() {
    // Escape sequence at end of properly quoted string
    assert_eq!(git::unquote_porcelain_path("\"end\\n\""), "end\n");
}

/// Test unquote_porcelain_path with unknown escape sequence
#[test]
fn adversarial_unquote_unknown_escape() {
    // Unknown escape \x should keep the backslash
    assert_eq!(
        git::unquote_porcelain_path("\"unknown\\xescape\""),
        "unknown\\xescape"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 2: Path traversal attempts in file paths
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with path traversal sequences
#[test]
fn adversarial_unquote_path_traversal() {
    // Path traversal should be passed through as-is (git handles this)
    assert_eq!(
        git::unquote_porcelain_path("\"../../../etc/passwd\""),
        "../../../etc/passwd"
    );
    assert_eq!(
        git::unquote_porcelain_path("\"..\\..\\..\\windows\\system32\""),
        "..\\..\\..\\windows\\system32"
    );
}

/// Test unquote_porcelain_path with mixed path traversal and normal chars
#[test]
fn adversarial_unquote_mixed_traversal() {
    assert_eq!(
        git::unquote_porcelain_path("\"foo/../../../bar\""),
        "foo/../../../bar"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 3: Unicode edge cases (emoji, RTL override, null bytes)
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with emoji in path
#[test]
fn adversarial_unquote_emoji() {
    // Emoji should pass through
    assert_eq!(
        git::unquote_porcelain_path("\"file_🎉_name.txt\""),
        "file_🎉_name.txt"
    );
}

/// Test unquote_porcelain_path with various unicode characters
#[test]
fn adversarial_unquote_unicode() {
    // Various unicode scripts
    assert_eq!(git::unquote_porcelain_path("\"文件.txt\""), "文件.txt");
    assert_eq!(git::unquote_porcelain_path("\"файл.txt\""), "файл.txt");
    assert_eq!(git::unquote_porcelain_path("\"αρχείο.txt\""), "αρχείο.txt");
}

/// Test unquote_porcelain_path with RTL override character (U+202E)
#[test]
fn adversarial_unquote_rtl_override() {
    // RTL override could be used to spoof filenames
    // The function should pass it through (git handles this)
    let rtl = "\u{202E}";
    let input = format!("\"{}txt.exe\"", rtl);
    let result = git::unquote_porcelain_path(&input);
    assert!(result.contains(rtl), "RTL override should be preserved");
}

/// Test unquote_porcelain_path with zero-width characters
#[test]
fn adversarial_unquote_zero_width() {
    // Zero-width space U+200B
    let zwsp = "\u{200B}";
    let input = format!("\"file{}name.txt\"", zwsp);
    let result = git::unquote_porcelain_path(&input);
    assert!(
        result.contains(zwsp),
        "Zero-width space should be preserved"
    );

    // Zero-width joiner U+200D
    let zwj = "\u{200D}";
    let input2 = format!("\"file{}name.txt\"", zwj);
    let result2 = git::unquote_porcelain_path(&input2);
    assert!(
        result2.contains(zwj),
        "Zero-width joiner should be preserved"
    );
}

/// Test unquote_porcelain_path with control characters via escape
#[test]
fn adversarial_unquote_control_chars() {
    // Newline via escape
    assert_eq!(git::unquote_porcelain_path("\"file\\nname\""), "file\nname");
    // Tab via escape
    assert_eq!(git::unquote_porcelain_path("\"file\\tname\""), "file\tname");
    // Carriage return via escape
    assert_eq!(git::unquote_porcelain_path("\"file\\rname\""), "file\rname");
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 4: Extremely long paths (buffer overflow attempt)
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with very long path
#[test]
fn adversarial_unquote_long_path() {
    // Create a path that's 4096 characters (typical PATH_MAX)
    let long_segment = "a".repeat(255);
    let long_path = long_segment.repeat(16); // ~4080 chars
    let input = format!("\"{}.txt\"", long_path);

    let result = git::unquote_porcelain_path(&input);
    assert_eq!(result.len(), long_path.len() + 4); // +4 for ".txt"
}

/// Test unquote_porcelain_path with extremely long path (potential overflow)
#[test]
fn adversarial_unquote_extreme_length() {
    // Create a path that's 65536 characters
    let long_path = "x".repeat(65536);
    let input = format!("\"{}\"", long_path);

    let result = git::unquote_porcelain_path(&input);
    assert_eq!(result.len(), 65536);
}

/// Test unquote_porcelain_path with many escape sequences
#[test]
fn adversarial_unquote_many_escapes() {
    // 1000 escaped newlines
    let escapes: String = "\\n".repeat(1000);
    let input = format!("\"{}\"", escapes);

    let result = git::unquote_porcelain_path(&input);
    assert_eq!(result.len(), 1000); // Each \n becomes single \n
    assert!(result.chars().all(|c| c == '\n'));
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 5: Empty/null cwd path
// ---------------------------------------------------------------------------

/// Test get_status_porcelain with non-existent directory
#[test]
fn adversarial_status_nonexistent_cwd() {
    let result = git::get_status_porcelain(Path::new("/nonexistent/path/that/does/not/exist"));
    assert!(result.is_err(), "Should fail for non-existent directory");
}

/// Test get_status_porcelain with non-git directory
#[test]
fn adversarial_status_non_git_dir() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let result = git::get_status_porcelain(temp_dir.path());
    assert!(result.is_err(), "Should fail for non-git directory");
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 6: Invalid UTF-8 handling (via escape sequences)
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path handles all valid escape sequences
#[test]
fn adversarial_unquote_all_escapes() {
    // Test all recognized escape sequences
    assert_eq!(git::unquote_porcelain_path("\"\\n\""), "\n");
    assert_eq!(git::unquote_porcelain_path("\"\\t\""), "\t");
    assert_eq!(git::unquote_porcelain_path("\"\\r\""), "\r");
    assert_eq!(git::unquote_porcelain_path("\"\\\"\""), "\"");
    assert_eq!(git::unquote_porcelain_path("\"\\\\\""), "\\");
}

/// Test unquote_porcelain_path with mixed escape and normal chars
#[test]
fn adversarial_unquote_mixed_escapes() {
    assert_eq!(
        git::unquote_porcelain_path("\"path\\tto\\nfile\\r\\\"name\\\"\""),
        "path\tto\nfile\r\"name\""
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 7: Deeply nested paths
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with deeply nested path
#[test]
fn adversarial_unquote_deeply_nested() {
    // Create a deeply nested path
    let nested: String = (0..100).map(|_| "dir/").collect::<String>();
    let path = format!("\"{}file.txt\"", nested);

    let result = git::unquote_porcelain_path(&path);
    assert!(result.starts_with("dir/dir/dir"));
    assert!(result.ends_with("file.txt"));
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 8: Edge cases in porcelain format parsing
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with spaces (unquoted)
#[test]
fn adversarial_unquote_unquoted_spaces() {
    // Unquoted path with spaces should be returned as-is
    assert_eq!(
        git::unquote_porcelain_path("file with spaces.txt"),
        "file with spaces.txt"
    );
}

/// Test unquote_porcelain_path with special characters unquoted
#[test]
fn adversarial_unquote_special_unquoted() {
    // These would normally be quoted by git, but test handling anyway
    assert_eq!(
        git::unquote_porcelain_path("file\ttab.txt"),
        "file\ttab.txt"
    );
    assert_eq!(
        git::unquote_porcelain_path("file\nnewline.txt"),
        "file\nnewline.txt"
    );
}

/// Test unquote_porcelain_path with only opening quote
#[test]
fn adversarial_unquote_only_opening_quote() {
    assert_eq!(
        git::unquote_porcelain_path("\"only_opening"),
        "\"only_opening"
    );
}

/// Test unquote_porcelain_path with only closing quote
#[test]
fn adversarial_unquote_only_closing_quote() {
    assert_eq!(
        git::unquote_porcelain_path("only_closing\""),
        "only_closing\""
    );
}

/// Test unquote_porcelain_path with quotes in middle (unquoted)
#[test]
fn adversarial_unquote_middle_quotes() {
    assert_eq!(
        git::unquote_porcelain_path("file\"middle\"name.txt"),
        "file\"middle\"name.txt"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 9: Binary-like content via escape sequences
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path produces valid strings from escape-heavy input
#[test]
fn adversarial_unquote_escape_heavy() {
    // Mix of all escape types
    let input = "\"\\n\\t\\r\\\"\\\\\\n\\t\"";
    let result = git::unquote_porcelain_path(input);
    assert_eq!(result, "\n\t\r\"\\\n\t");
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 10: Potential injection via special characters
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with shell metacharacters
#[test]
fn adversarial_unquote_shell_metacharacters() {
    // Shell metacharacters should pass through (git handles escaping)
    assert_eq!(
        git::unquote_porcelain_path("\"file;rm -rf /\""),
        "file;rm -rf /"
    );
    assert_eq!(git::unquote_porcelain_path("\"$(whoami)\""), "$(whoami)");
    assert_eq!(git::unquote_porcelain_path("\"`id`\""), "`id`");
    assert_eq!(git::unquote_porcelain_path("\"file|cat\""), "file|cat");
    assert_eq!(git::unquote_porcelain_path("\"file>out\""), "file>out");
    assert_eq!(git::unquote_porcelain_path("\"file<in\""), "file<in");
    assert_eq!(git::unquote_porcelain_path("\"file&bg\""), "file&bg");
}

// ---------------------------------------------------------------------------
// Real-world adversarial file names via git
// ---------------------------------------------------------------------------

/// Test real file with leading dash (could be interpreted as option)
#[test]
fn adversarial_real_file_leading_dash() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with leading dash
    let dash_file = temp_git.path().join("-dangerous.txt");
    File::create(&dash_file)
        .unwrap()
        .write_all(b"content")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = entries
        .iter()
        .find(|e| e.change.path().to_string_lossy().contains("-dangerous.txt"))
        .expect("Should find file with leading dash");

    assert!(matches!(entry.change, crate::FileChange::Untracked { .. }));
}

/// Test real file with newlines in name (via git)
#[test]
fn adversarial_real_file_newline_in_name() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with newline in name (this will be quoted by git)
    let newline_file = temp_git.path().join("file\nwith\nnewlines.txt");
    File::create(&newline_file)
        .unwrap()
        .write_all(b"content")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    // Should find the file with actual newlines in path
    let entry = entries
        .iter()
        .find(|e| e.change.path().to_string_lossy().contains('\n'))
        .expect("Should find file with newlines");

    assert!(matches!(entry.change, crate::FileChange::Untracked { .. }));
}

/// Test real file with backslash in name
#[test]
#[cfg(unix)]
fn adversarial_real_file_backslash_in_name() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // On Unix, backslash is a valid filename character
    let backslash_file = temp_git.path().join("file\\backslash.txt");
    File::create(&backslash_file)
        .unwrap()
        .write_all(b"content")
        .unwrap();

    let entries = git::get_status_porcelain(temp_git.path()).unwrap();

    let entry = entries
        .iter()
        .find(|e| e.change.path().to_string_lossy().contains('\\'))
        .expect("Should find file with backslash");

    assert!(matches!(entry.change, crate::FileChange::Untracked { .. }));
}

/// Test real file with null-byte-like pattern (escaped)
#[test]
fn adversarial_unquote_null_byte_pattern() {
    // Git doesn't allow actual null bytes, but test the escape handling
    // Note: \0 is not a standard git escape, so it should be passed through
    assert_eq!(
        git::unquote_porcelain_path("\"file\\0null\""),
        "file\\0null"
    );
}

// ---------------------------------------------------------------------------
// Fuzz-like tests: Random/chaotic input patterns
// ---------------------------------------------------------------------------

/// Test unquote_porcelain_path with alternating escapes
#[test]
fn adversarial_unquote_alternating_escapes() {
    let input = "\"\\n\\t\\n\\t\\n\\t\\n\\t\"";
    let result = git::unquote_porcelain_path(input);
    assert_eq!(result, "\n\t\n\t\n\t\n\t");
}

/// Test unquote_porcelain_path with consecutive backslashes
#[test]
fn adversarial_unquote_consecutive_backslashes() {
    // \\\\ should become \\
    assert_eq!(git::unquote_porcelain_path("\"\\\\\\\\\""), "\\\\");
    // \\\\\\ should become \\\
    assert_eq!(git::unquote_porcelain_path("\"\\\\\\\\\\\\\""), "\\\\\\");
}

/// Test unquote_porcelain_path with escape at boundary
#[test]
fn adversarial_unquote_escape_at_boundary() {
    // Escape sequence at very end
    assert_eq!(git::unquote_porcelain_path("\"end\\n\""), "end\n");
    assert_eq!(git::unquote_porcelain_path("\"end\\t\""), "end\t");
}

/// Test unquote_porcelain_path with empty string
#[test]
fn adversarial_unquote_empty_string() {
    assert_eq!(git::unquote_porcelain_path(""), "");
}

/// Test unquote_porcelain_path with single character
#[test]
fn adversarial_unquote_single_char() {
    assert_eq!(git::unquote_porcelain_path("a"), "a");
}

/// Test unquote_porcelain_path with only backslash
#[test]
fn adversarial_unquote_only_backslash() {
    assert_eq!(git::unquote_porcelain_path("\\"), "\\");
}

/// Test unquote_porcelain_path with only quote
#[test]
fn adversarial_unquote_only_quote() {
    assert_eq!(git::unquote_porcelain_path("\""), "\"");
}
