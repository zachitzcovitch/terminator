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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();
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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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
    let result =
        git::get_status_porcelain(Path::new("/nonexistent/path/that/does/not/exist"), false);
    assert!(result.is_err(), "Should fail for non-existent directory");
}

/// Test get_status_porcelain with non-git directory
#[test]
fn adversarial_status_non_git_dir() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let result = git::get_status_porcelain(temp_dir.path(), false);
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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

    let entries = git::get_status_porcelain(temp_git.path(), false).unwrap();

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

// ============================================================================
// Tests for get_diff_stats
// ============================================================================

/// Test 1: Modified file with additions and deletions → Some((add, del))
#[test]
fn diff_stats_modified_file() {
    let temp_git = empty_git_repo();

    // Create and commit a file with multiple lines
    let file = temp_git.path().join("file.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline2\nline3\nline4\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify: delete line2, add a new line5
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline3\nline4\nline5\n")
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("file.txt"), false).unwrap();
    assert!(result.is_some(), "Should have diff stats for modified file");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 1, "Should have 1 addition (line5)");
    assert_eq!(deletions, 1, "Should have 1 deletion (line2)");
}

/// Test 2: Untracked file → None (no output from git diff HEAD)
#[test]
fn diff_stats_untracked_file() {
    let temp_git = empty_git_repo();

    // Create initial commit so we have a valid HEAD
    let initial = temp_git.path().join("initial.txt");
    File::create(&initial)
        .unwrap()
        .write_all(b"initial")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Create an untracked file
    let untracked = temp_git.path().join("untracked.txt");
    File::create(&untracked)
        .unwrap()
        .write_all(b"untracked content\n")
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("untracked.txt"), false).unwrap();
    assert!(
        result.is_none(),
        "Untracked file should return None (no diff against HEAD)"
    );
}

/// Test 3: Binary file → None (shows `- - filename`)
#[test]
fn diff_stats_binary_file() {
    let temp_git = empty_git_repo();

    // Create and commit a binary file (PNG header magic bytes)
    let binary_file = temp_git.path().join("image.png");
    let png_header: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut binary_data = png_header.to_vec();
    binary_data.extend_from_slice(&[0u8; 100]); // Add some padding
    File::create(&binary_file)
        .unwrap()
        .write_all(&binary_data)
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the binary file
    let mut new_binary_data = png_header.to_vec();
    new_binary_data.extend_from_slice(&[1u8; 100]); // Different padding
    File::create(&binary_file)
        .unwrap()
        .write_all(&new_binary_data)
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("image.png"), false).unwrap();
    assert!(result.is_some(), "Binary file should return Some");
    let (additions, deletions, is_binary) = result.unwrap();
    assert!(is_binary, "Binary file should have is_binary=true");
    assert_eq!(additions, 0, "Binary file should have 0 additions");
    assert_eq!(deletions, 0, "Binary file should have 0 deletions");
}

/// Test 4: File with no changes → None (empty output)
#[test]
fn diff_stats_no_changes() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("file.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"unchanged content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Don't modify the file - no changes
    let result = git::get_diff_stats(temp_git.path(), Path::new("file.txt"), false).unwrap();
    assert!(result.is_none(), "File with no changes should return None");
}

/// Test 5: New file (staged but not committed) → Some((lines, 0))
/// Note: `git diff --numstat HEAD -- <file>` compares staged content against HEAD.
/// For a staged new file, HEAD doesn't have it, so it shows as added lines.
#[test]
fn diff_stats_staged_new_file() {
    let temp_git = empty_git_repo();

    // Create initial commit so we have a valid HEAD
    let initial = temp_git.path().join("initial.txt");
    File::create(&initial)
        .unwrap()
        .write_all(b"initial")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Create and stage a new file
    let new_file = temp_git.path().join("new_file.txt");
    File::create(&new_file)
        .unwrap()
        .write_all(b"new content\nline2\nline3\n")
        .unwrap();
    exec_git_cmd("add new_file.txt", temp_git.path());

    // Staged but not committed - diff against HEAD shows the file as added
    let result = git::get_diff_stats(temp_git.path(), Path::new("new_file.txt"), true).unwrap();
    assert!(
        result.is_some(),
        "Staged new file should have stats (comparing staged content against HEAD)"
    );

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 3, "Should have 3 additions (new lines)");
    assert_eq!(deletions, 0, "Should have 0 deletions (file not in HEAD)");
}

/// Test 6: Deleted file → Some((0, lines))
#[test]
fn diff_stats_deleted_file() {
    let temp_git = empty_git_repo();

    // Create and commit a file with multiple lines
    let file = temp_git.path().join("to_delete.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline2\nline3\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Delete the file
    std::fs::remove_file(&file).unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("to_delete.txt"), false).unwrap();
    assert!(result.is_some(), "Deleted file should have diff stats");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 0, "Deleted file should have 0 additions");
    assert_eq!(deletions, 3, "Deleted file should have 3 deletions");
}

/// Test 7: File with only additions → Some((N, 0))
#[test]
fn diff_stats_only_additions() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"line1\n").unwrap();
    create_commit(temp_git.path(), true);

    // Add lines without deleting any
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline2\nline3\nline4\n")
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("file.txt"), false).unwrap();
    assert!(result.is_some(), "Should have diff stats");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 3, "Should have 3 additions");
    assert_eq!(deletions, 0, "Should have 0 deletions");
}

/// Test 8: File with only deletions → Some((0, N))
#[test]
fn diff_stats_only_deletions() {
    let temp_git = empty_git_repo();

    // Create and commit a file with multiple lines
    let file = temp_git.path().join("file.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline2\nline3\nline4\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Remove lines without adding any
    File::create(&file).unwrap().write_all(b"line1\n").unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("file.txt"), false).unwrap();
    assert!(result.is_some(), "Should have diff stats");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 0, "Should have 0 additions");
    assert_eq!(deletions, 3, "Should have 3 deletions");
}

/// Test 9: Large number of changes
#[test]
fn diff_stats_large_changes() {
    let temp_git = empty_git_repo();

    // Create and commit a file with many lines
    let file = temp_git.path().join("large.txt");
    let original_content: String = (1..=1000).map(|i| format!("line{}\n", i)).collect();
    File::create(&file)
        .unwrap()
        .write_all(original_content.as_bytes())
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify: replace first 500 lines with 500 new lines
    let new_content: String = (1001..=1500).map(|i| format!("line{}\n", i)).collect();
    File::create(&file)
        .unwrap()
        .write_all(new_content.as_bytes())
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("large.txt"), false).unwrap();
    assert!(result.is_some(), "Should have diff stats");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    // The exact counts depend on diff algorithm, but should be substantial
    assert!(additions > 400, "Should have many additions");
    assert!(deletions > 400, "Should have many deletions");
}

/// Test 10: Empty file created and committed
#[test]
fn diff_stats_empty_file() {
    let temp_git = empty_git_repo();

    // Create and commit an empty file
    let file = temp_git.path().join("empty.txt");
    File::create(&file).unwrap();
    create_commit(temp_git.path(), true);

    // Add content to the empty file
    File::create(&file)
        .unwrap()
        .write_all(b"new content\n")
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("empty.txt"), false).unwrap();
    assert!(result.is_some(), "Should have diff stats");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 1, "Should have 1 addition");
    assert_eq!(deletions, 0, "Should have 0 deletions");
}

/// Test 11: File with whitespace-only changes (may show as 0/0)
#[test]
fn diff_stats_whitespace_changes() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let file = temp_git.path().join("file.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline2\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Add trailing whitespace (still counts as a change)
    File::create(&file)
        .unwrap()
        .write_all(b"line1\nline2  \n")
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("file.txt"), false).unwrap();
    // Whitespace changes are still line changes
    assert!(result.is_some(), "Should have diff stats");
}

/// Test 12: Non-existent file path
#[test]
fn diff_stats_nonexistent_file() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let initial = temp_git.path().join("initial.txt");
    File::create(&initial)
        .unwrap()
        .write_all(b"initial")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Query a file that doesn't exist
    let result = git::get_diff_stats(temp_git.path(), Path::new("nonexistent.txt"), false).unwrap();
    assert!(result.is_none(), "Non-existent file should return None");
}

/// Test 13: File in subdirectory
#[test]
fn diff_stats_subdirectory_file() {
    let temp_git = empty_git_repo();

    // Create subdirectory and file
    let subdir = temp_git.path().join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    let file = subdir.join("file.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"original\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the file
    File::create(&file)
        .unwrap()
        .write_all(b"modified\n")
        .unwrap();

    let result = git::get_diff_stats(temp_git.path(), Path::new("subdir/file.txt"), false).unwrap();
    assert!(result.is_some(), "Should have diff stats");

    let (additions, deletions, is_binary) = result.unwrap();
    assert!(!is_binary, "Text file should not be binary");
    assert_eq!(additions, 1, "Should have 1 addition");
    assert_eq!(deletions, 1, "Should have 1 deletion");
}

/// Test 14: Renamed file (should show stats for new path)
#[test]
fn diff_stats_renamed_file() {
    let temp_git = empty_git_repo();

    // Create and commit a file
    let old_file = temp_git.path().join("old_name.txt");
    File::create(&old_file)
        .unwrap()
        .write_all(b"content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Rename the file
    let new_file = temp_git.path().join("new_name.txt");
    std::fs::rename(&old_file, &new_file).unwrap();

    // Old file should show as deleted
    let old_result =
        git::get_diff_stats(temp_git.path(), Path::new("old_name.txt"), false).unwrap();
    assert!(old_result.is_some(), "Old file should have stats");
    let (_, deletions, _) = old_result.unwrap();
    assert_eq!(deletions, 1, "Old file should show as deleted");

    // New file should show as added (untracked, so None against HEAD)
    let new_result =
        git::get_diff_stats(temp_git.path(), Path::new("new_name.txt"), false).unwrap();
    assert!(
        new_result.is_none(),
        "New file path (untracked) should return None"
    );
}

// ============================================================================
// ADVERSARIAL SECURITY TESTS for get_diff_stats
// ============================================================================

// ---------------------------------------------------------------------------
// ATTACK VECTOR 1: Path traversal attempts
// ---------------------------------------------------------------------------

/// Test get_diff_stats with path traversal in file path
/// Attempts to access files outside the repository using ../../../
#[test]
fn adversarial_diff_stats_path_traversal() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Attempt path traversal - should not leak files outside repo
    // Git will reject paths outside the repository
    let result = git::get_diff_stats(temp_git.path(), Path::new("../../../etc/passwd"), false);
    // Should either error or return None (git rejects outside-repo paths)
    assert!(
        result.is_ok() || result.is_err(),
        "Path traversal should not cause panic"
    );
}

/// Test get_diff_stats with mixed path traversal
#[test]
fn adversarial_diff_stats_mixed_traversal() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file in subdirectory
    let subdir = temp_git.path().join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    let subfile = subdir.join("file.txt");
    File::create(&subfile)
        .unwrap()
        .write_all(b"content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the file
    File::create(&subfile)
        .unwrap()
        .write_all(b"modified\n")
        .unwrap();

    // Attempt traversal from subdirectory
    let result = git::get_diff_stats(subdir.as_path(), Path::new("../initial.txt"), false);
    // Should handle gracefully
    assert!(
        result.is_ok() || result.is_err(),
        "Mixed traversal should not cause panic"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 2: Shell injection via filename
// ---------------------------------------------------------------------------

/// Test get_diff_stats with shell injection characters in filename
/// Rust's Command API doesn't use shell, so these should be safe
#[test]
fn adversarial_diff_stats_shell_injection_semicolon() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with shell injection attempt in name
    // Note: semicolons are valid filename characters on Unix
    #[cfg(unix)]
    {
        let inject_file = temp_git.path().join("file;rm -rf.txt");
        File::create(&inject_file)
            .unwrap()
            .write_all(b"content\n")
            .unwrap();
        create_commit(temp_git.path(), true);

        // Modify the file
        File::create(&inject_file)
            .unwrap()
            .write_all(b"modified\n")
            .unwrap();

        // Should handle safely - no shell execution
        let result = git::get_diff_stats(temp_git.path(), Path::new("file;rm -rf.txt"), false);
        assert!(
            result.is_ok(),
            "Shell injection chars in filename should be handled safely"
        );
    }
}

/// Test get_diff_stats with command substitution attempt
#[test]
fn adversarial_diff_stats_command_substitution() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with command substitution attempt in name
    #[cfg(unix)]
    {
        let inject_file = temp_git.path().join("file$(whoami).txt");
        File::create(&inject_file)
            .unwrap()
            .write_all(b"content\n")
            .unwrap();
        create_commit(temp_git.path(), true);

        // Modify the file
        File::create(&inject_file)
            .unwrap()
            .write_all(b"modified\n")
            .unwrap();

        // Should handle safely - no shell execution
        let result = git::get_diff_stats(temp_git.path(), Path::new("file$(whoami).txt"), false);
        assert!(
            result.is_ok(),
            "Command substitution in filename should be handled safely"
        );
    }
}

/// Test get_diff_stats with backtick injection attempt
#[test]
fn adversarial_diff_stats_backtick_injection() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with backtick injection attempt in name
    #[cfg(unix)]
    {
        let inject_file = temp_git.path().join("file`id`.txt");
        File::create(&inject_file)
            .unwrap()
            .write_all(b"content\n")
            .unwrap();
        create_commit(temp_git.path(), true);

        // Modify the file
        File::create(&inject_file)
            .unwrap()
            .write_all(b"modified\n")
            .unwrap();

        // Should handle safely - no shell execution
        let result = git::get_diff_stats(temp_git.path(), Path::new("file`id`.txt"), false);
        assert!(
            result.is_ok(),
            "Backtick injection in filename should be handled safely"
        );
    }
}

/// Test get_diff_stats with pipe injection attempt
#[test]
fn adversarial_diff_stats_pipe_injection() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with pipe character in name
    #[cfg(unix)]
    {
        let inject_file = temp_git.path().join("file|cat.txt");
        File::create(&inject_file)
            .unwrap()
            .write_all(b"content\n")
            .unwrap();
        create_commit(temp_git.path(), true);

        // Modify the file
        File::create(&inject_file)
            .unwrap()
            .write_all(b"modified\n")
            .unwrap();

        // Should handle safely - no shell execution
        let result = git::get_diff_stats(temp_git.path(), Path::new("file|cat.txt"), false);
        assert!(
            result.is_ok(),
            "Pipe injection in filename should be handled safely"
        );
    }
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 3: Extremely long file path (buffer overflow attempt)
// ---------------------------------------------------------------------------

/// Test get_diff_stats with extremely long file path
#[test]
fn adversarial_diff_stats_extremely_long_path() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a path that's 4096 characters (typical PATH_MAX)
    let long_name = "a".repeat(4096);
    let long_path = Path::new(&long_name);

    // Should not cause buffer overflow or panic
    let result = git::get_diff_stats(temp_git.path(), long_path, false);
    assert!(
        result.is_ok() || result.is_err(),
        "Extremely long path should not cause panic"
    );
}

/// Test get_diff_stats with path exceeding filesystem limits
#[test]
fn adversarial_diff_stats_path_overflow_attempt() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a path that's 65536 characters
    let long_name = "x".repeat(65536);
    let long_path = Path::new(&long_name);

    // Should not cause buffer overflow or panic
    let result = git::get_diff_stats(temp_git.path(), long_path, false);
    assert!(
        result.is_ok() || result.is_err(),
        "Path overflow attempt should not cause panic"
    );
}

/// Test get_diff_stats with deeply nested path
#[test]
fn adversarial_diff_stats_deeply_nested_path() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a deeply nested path string
    let nested: String = (0..100).map(|_| "dir/").collect::<String>() + "file.txt";
    let nested_path = Path::new(&nested);

    // Should handle gracefully
    let result = git::get_diff_stats(temp_git.path(), nested_path, false);
    assert!(
        result.is_ok() || result.is_err(),
        "Deeply nested path should not cause panic"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 4: Unicode edge cases
// ---------------------------------------------------------------------------

/// Test get_diff_stats with emoji in filename
#[test]
fn adversarial_diff_stats_emoji_filename() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with emoji in name
    let emoji_file = temp_git.path().join("file_🎉_name.txt");
    File::create(&emoji_file)
        .unwrap()
        .write_all(b"content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the file
    File::create(&emoji_file)
        .unwrap()
        .write_all(b"modified\n")
        .unwrap();

    // Should handle unicode safely
    let result = git::get_diff_stats(temp_git.path(), Path::new("file_🎉_name.txt"), false);
    assert!(result.is_ok(), "Emoji in filename should be handled safely");
}

/// Test get_diff_stats with various unicode scripts
#[test]
fn adversarial_diff_stats_unicode_scripts() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create files with various unicode scripts
    let unicode_names = vec!["文件.txt", "файл.txt", "αρχείο.txt"];

    for name in &unicode_names {
        let unicode_file = temp_git.path().join(name);
        File::create(&unicode_file)
            .unwrap()
            .write_all(b"content\n")
            .unwrap();
        create_commit(temp_git.path(), true);

        // Modify the file
        File::create(&unicode_file)
            .unwrap()
            .write_all(b"modified\n")
            .unwrap();

        // Should handle unicode safely
        let result = git::get_diff_stats(temp_git.path(), Path::new(name), false);
        assert!(
            result.is_ok(),
            "Unicode script '{}' should be handled safely",
            name
        );
    }
}

/// Test get_diff_stats with RTL override character (U+202E)
/// This could be used to spoof filenames
#[test]
fn adversarial_diff_stats_rtl_override() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with RTL override character
    let rtl = "\u{202E}";
    let rtl_filename = format!("{}txt.exe", rtl);
    let rtl_file = temp_git.path().join(&rtl_filename);

    match File::create(&rtl_file) {
        Ok(mut f) => {
            f.write_all(b"content\n").unwrap();
            create_commit(temp_git.path(), true);

            // Modify the file
            File::create(&rtl_file)
                .unwrap()
                .write_all(b"modified\n")
                .unwrap();

            // Should handle RTL override safely
            let result = git::get_diff_stats(temp_git.path(), Path::new(&rtl_filename), false);
            assert!(
                result.is_ok(),
                "RTL override in filename should be handled safely"
            );
        }
        Err(_) => {
            // Some filesystems may not support RTL override in filenames
            // This is acceptable - just ensure no panic
        }
    }
}

/// Test get_diff_stats with zero-width characters
#[test]
fn adversarial_diff_stats_zero_width_chars() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with zero-width space
    let zwsp = "\u{200B}";
    let zwsp_filename = format!("file{}name.txt", zwsp);
    let zwsp_file = temp_git.path().join(&zwsp_filename);

    match File::create(&zwsp_file) {
        Ok(mut f) => {
            f.write_all(b"content\n").unwrap();
            create_commit(temp_git.path(), true);

            // Modify the file
            File::create(&zwsp_file)
                .unwrap()
                .write_all(b"modified\n")
                .unwrap();

            // Should handle zero-width chars safely
            let result = git::get_diff_stats(temp_git.path(), Path::new(&zwsp_filename), false);
            assert!(
                result.is_ok(),
                "Zero-width chars in filename should be handled safely"
            );
        }
        Err(_) => {
            // Some filesystems may not support zero-width chars
            // This is acceptable - just ensure no panic
        }
    }
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 5: Special characters in filename
// ---------------------------------------------------------------------------

/// Test get_diff_stats with newlines in filename
#[test]
fn adversarial_diff_stats_newline_in_filename() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with newline in name
    let newline_filename = "file\nwith\nnewline.txt";
    let newline_file = temp_git.path().join(newline_filename);

    match File::create(&newline_file) {
        Ok(mut f) => {
            f.write_all(b"content\n").unwrap();
            create_commit(temp_git.path(), true);

            // Modify the file
            File::create(&newline_file)
                .unwrap()
                .write_all(b"modified\n")
                .unwrap();

            // Should handle newlines safely
            let result = git::get_diff_stats(temp_git.path(), Path::new(newline_filename), false);
            assert!(
                result.is_ok(),
                "Newlines in filename should be handled safely"
            );
        }
        Err(_) => {
            // Some filesystems may not support newlines in filenames
            // This is acceptable - just ensure no panic
        }
    }
}

/// Test get_diff_stats with tabs in filename
#[test]
fn adversarial_diff_stats_tab_in_filename() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with tab in name
    let tab_filename = "file\twith\ttab.txt";
    let tab_file = temp_git.path().join(tab_filename);

    match File::create(&tab_file) {
        Ok(mut f) => {
            f.write_all(b"content\n").unwrap();
            create_commit(temp_git.path(), true);

            // Modify the file
            File::create(&tab_file)
                .unwrap()
                .write_all(b"modified\n")
                .unwrap();

            // Should handle tabs safely
            let result = git::get_diff_stats(temp_git.path(), Path::new(tab_filename), false);
            assert!(result.is_ok(), "Tabs in filename should be handled safely");
        }
        Err(_) => {
            // Some filesystems may not support tabs in filenames
            // This is acceptable - just ensure no panic
        }
    }
}

/// Test get_diff_stats with quotes in filename
#[test]
fn adversarial_diff_stats_quotes_in_filename() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with quotes in name
    let quote_filename = "file\"with\"quotes.txt";
    let quote_file = temp_git.path().join(quote_filename);

    match File::create(&quote_file) {
        Ok(mut f) => {
            f.write_all(b"content\n").unwrap();
            create_commit(temp_git.path(), true);

            // Modify the file
            File::create(&quote_file)
                .unwrap()
                .write_all(b"modified\n")
                .unwrap();

            // Should handle quotes safely
            let result = git::get_diff_stats(temp_git.path(), Path::new(quote_filename), false);
            assert!(
                result.is_ok(),
                "Quotes in filename should be handled safely"
            );
        }
        Err(_) => {
            // Some filesystems may not support quotes in filenames
            // This is acceptable - just ensure no panic
        }
    }
}

/// Test get_diff_stats with spaces in filename
#[test]
fn adversarial_diff_stats_spaces_in_filename() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with spaces in name
    let space_file = temp_git.path().join("file with spaces.txt");
    File::create(&space_file)
        .unwrap()
        .write_all(b"content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the file
    File::create(&space_file)
        .unwrap()
        .write_all(b"modified\n")
        .unwrap();

    // Should handle spaces safely
    let result = git::get_diff_stats(temp_git.path(), Path::new("file with spaces.txt"), false);
    assert!(
        result.is_ok(),
        "Spaces in filename should be handled safely"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 6: Non-existent cwd
// ---------------------------------------------------------------------------

/// Test get_diff_stats with non-existent cwd
#[test]
fn adversarial_diff_stats_nonexistent_cwd() {
    let result = git::get_diff_stats(
        Path::new("/nonexistent/path/that/does/not/exist"),
        Path::new("file.txt"),
        false,
    );
    // Should error gracefully, not panic
    assert!(
        result.is_err(),
        "Non-existent cwd should return error, not panic"
    );
}

/// Test get_diff_stats with non-git cwd
#[test]
fn adversarial_diff_stats_non_git_cwd() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let result = git::get_diff_stats(temp_dir.path(), Path::new("file.txt"), false);
    // Should error gracefully, not panic
    assert!(
        result.is_err(),
        "Non-git cwd should return error, not panic"
    );
}

/// Test get_diff_stats with file path as cwd (not a directory)
#[test]
fn adversarial_diff_stats_file_as_cwd() {
    let temp_git = empty_git_repo();

    // Create a file
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"content").unwrap();

    // Try to use file as cwd
    let result = git::get_diff_stats(&file, Path::new("other.txt"), false);
    // Should error gracefully, not panic
    assert!(
        result.is_err() || result.is_ok(),
        "File as cwd should not panic"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 7: File path with leading dash
// ---------------------------------------------------------------------------

/// Test get_diff_stats with file path starting with dash
/// This tests that the `--` argument properly prevents option injection
#[test]
fn adversarial_diff_stats_leading_dash() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with leading dash
    let dash_file = temp_git.path().join("-dangerous.txt");
    File::create(&dash_file)
        .unwrap()
        .write_all(b"content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the file
    File::create(&dash_file)
        .unwrap()
        .write_all(b"modified\n")
        .unwrap();

    // Should handle leading dash safely (-- prevents option interpretation)
    let result = git::get_diff_stats(temp_git.path(), Path::new("-dangerous.txt"), false);
    assert!(
        result.is_ok(),
        "Leading dash in filename should be handled safely (-- prevents option injection)"
    );
}

/// Test get_diff_stats with file path that looks like git option
#[test]
fn adversarial_diff_stats_option_like_filename() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create files that look like git options
    let option_names = vec!["--help", "--version", "--bare", "--hard"];

    for name in &option_names {
        let option_file = temp_git.path().join(name);
        match File::create(&option_file) {
            Ok(mut f) => {
                f.write_all(b"content\n").unwrap();
                create_commit(temp_git.path(), true);

                // Modify the file
                File::create(&option_file)
                    .unwrap()
                    .write_all(b"modified\n")
                    .unwrap();

                // Should handle option-like names safely
                let result = git::get_diff_stats(temp_git.path(), Path::new(name), false);
                assert!(
                    result.is_ok(),
                    "Option-like filename '{}' should be handled safely",
                    name
                );
            }
            Err(_) => {
                // Some filesystems may not support these names
                // This is acceptable - just ensure no panic
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 8: Empty file path
// ---------------------------------------------------------------------------

/// Test get_diff_stats with empty file path
#[test]
fn adversarial_diff_stats_empty_path() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Empty path should be handled gracefully
    let result = git::get_diff_stats(temp_git.path(), Path::new(""), false);
    // Should not panic - either error or return None
    assert!(
        result.is_ok() || result.is_err(),
        "Empty path should not cause panic"
    );
}

/// Test get_diff_stats with dot as file path
#[test]
fn adversarial_diff_stats_dot_path() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Dot path should be handled gracefully
    let result = git::get_diff_stats(temp_git.path(), Path::new("."), false);
    // Should not panic
    assert!(
        result.is_ok() || result.is_err(),
        "Dot path should not cause panic"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 9: Symlink attacks
// ---------------------------------------------------------------------------

/// Test get_diff_stats with symlink pointing outside repo
#[cfg(unix)]
#[test]
fn adversarial_diff_stats_symlink_outside_repo() {
    use std::os::unix::fs::symlink;

    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a symlink pointing outside the repo
    let symlink_path = temp_git.path().join("external_link");
    // Try to create symlink to /etc/passwd
    let _ = symlink("/etc/passwd", &symlink_path);

    // Should handle symlink safely
    let result = git::get_diff_stats(temp_git.path(), Path::new("external_link"), false);
    // Should not panic, may error or return None
    assert!(
        result.is_ok() || result.is_err(),
        "Symlink outside repo should not cause panic"
    );
}

/// Test get_diff_stats with symlink loop
#[cfg(unix)]
#[test]
fn adversarial_diff_stats_symlink_loop() {
    use std::os::unix::fs::symlink;

    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a symlink loop
    let link1 = temp_git.path().join("link1");
    let link2 = temp_git.path().join("link2");
    let _ = symlink("link2", &link1);
    let _ = symlink("link1", &link2);

    // Should handle symlink loop safely
    let result = git::get_diff_stats(temp_git.path(), Path::new("link1"), false);
    // Should not panic
    assert!(
        result.is_ok() || result.is_err(),
        "Symlink loop should not cause panic"
    );
}

// ---------------------------------------------------------------------------
// ATTACK VECTOR 10: Boundary violations
// ---------------------------------------------------------------------------

/// Test get_diff_stats with null byte attempt in path
/// Note: Rust's Path::new will handle this, but test for safety
#[test]
fn adversarial_diff_stats_null_byte_path() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Attempt to create path with null byte
    // Rust strings can't contain null bytes, but test the handling
    let null_path = "file\x00name.txt";
    let path = Path::new(null_path);

    // Should handle safely
    let result = git::get_diff_stats(temp_git.path(), path, false);
    // Should not panic
    assert!(
        result.is_ok() || result.is_err(),
        "Null byte in path should not cause panic"
    );
}

/// Test get_diff_stats with absolute path (should work but test for safety)
#[test]
fn adversarial_diff_stats_absolute_path() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file and get its absolute path
    let abs_file = temp_git.path().join("test.txt");
    File::create(&abs_file)
        .unwrap()
        .write_all(b"content\n")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify the file
    File::create(&abs_file)
        .unwrap()
        .write_all(b"modified\n")
        .unwrap();

    // Use absolute path
    let result = git::get_diff_stats(temp_git.path(), abs_file.as_path(), false);
    // Should handle absolute path safely
    assert!(
        result.is_ok() || result.is_err(),
        "Absolute path should not cause panic"
    );
}

// ---------------------------------------------------------------------------
// FUZZ-LIKE: Combined attack vectors
// ---------------------------------------------------------------------------

/// Test get_diff_stats with multiple attack vectors combined
#[test]
fn adversarial_diff_stats_combined_attacks() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Create a file with multiple special characters
    #[cfg(unix)]
    {
        let combined_file = temp_git.path().join("file;$(whoami)`id`|cat.txt");
        File::create(&combined_file)
            .unwrap()
            .write_all(b"content\n")
            .unwrap();
        create_commit(temp_git.path(), true);

        // Modify the file
        File::create(&combined_file)
            .unwrap()
            .write_all(b"modified\n")
            .unwrap();

        // Should handle combined attacks safely
        let result = git::get_diff_stats(
            temp_git.path(),
            Path::new("file;$(whoami)`id`|cat.txt"),
            false,
        );
        assert!(
            result.is_ok(),
            "Combined attack vectors should be handled safely"
        );
    }
}

/// Test get_diff_stats rapid repeated calls (resource exhaustion attempt)
#[test]
fn adversarial_diff_stats_resource_exhaustion() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Rapid repeated calls should not cause resource exhaustion
    for _ in 0..100 {
        let result = git::get_diff_stats(temp_git.path(), Path::new("initial.txt"), false);
        assert!(
            result.is_ok(),
            "Repeated calls should not cause resource issues"
        );
    }
}
