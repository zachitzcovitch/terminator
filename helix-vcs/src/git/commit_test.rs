use std::{fs::File, io::Write, path::Path, process::Command};

use tempfile::TempDir;

use crate::git;

fn exec_git_cmd(args: &str, git_dir: &Path) {
    let res = Command::new("git")
        .arg("-C")
        .arg(git_dir)
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

// ============================================================================
// Tests for commit function (Task 8.5: Commit Workflow)
// ============================================================================

/// Test 1: commit with no staged files should fail
/// Verifies: `c` key with no staged files shows error
#[test]
fn commit_no_staged_files() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify file but don't stage it
    File::create(&file).unwrap().write_all(b"modified").unwrap();

    // Attempt to commit should fail because nothing is staged
    let result = git::commit(temp_git.path(), "test message");
    assert!(
        result.is_err(),
        "Commit should fail when there are no staged files"
    );
    let err = result.unwrap_err().to_string();
    // Git may output the error to stdout or stderr depending on version
    assert!(
        err.contains("nothing to commit")
            || err.contains("no changes added")
            || err.contains("git commit failed"),
        "Error should indicate commit failure, got: {err}"
    );
}

/// Test 2: commit with staged files should succeed
/// Verifies: `c` key with staged files opens commit prompt and commits successfully
#[test]
fn commit_with_staged_files() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add initial.txt", temp_git.path());

    // Commit should succeed
    let result = git::commit(temp_git.path(), "test commit message");
    assert!(
        result.is_ok(),
        "Commit should succeed with staged files: {:?}",
        result.err()
    );

    // Verify the commit was created with the correct message
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["log", "-1", "--format=%s"])
        .output()
        .expect("Failed to get git log");
    let message = String::from_utf8_lossy(&output.stdout);
    assert!(
        message.trim() == "test commit message",
        "Commit message should match, got: {message}"
    );
}

/// Test 3: commit message with special characters should work
/// Verifies: commit message is properly passed through stdin
#[test]
fn commit_with_special_characters() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add initial.txt", temp_git.path());

    // Commit with special characters in message
    let special_message = "fix: handle 'quotes' and \"double quotes\" + $var `code`";
    let result = git::commit(temp_git.path(), special_message);
    assert!(
        result.is_ok(),
        "Commit with special characters should succeed: {:?}",
        result.err()
    );

    // Verify the commit message
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["log", "-1", "--format=%s"])
        .output()
        .expect("Failed to get git log");
    let message = String::from_utf8_lossy(&output.stdout);
    assert!(
        message.trim() == special_message,
        "Commit message should match with special characters, got: {message}"
    );
}

/// Test 4: commit message with newlines should work
/// Verifies: multiline commit messages are properly handled via stdin
#[test]
fn commit_with_multiline_message() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add initial.txt", temp_git.path());

    // Commit with multiline message
    let multiline_message =
        "feat: add new feature\n\nThis is the body of the commit message.\nIt has multiple lines.";
    let result = git::commit(temp_git.path(), multiline_message);
    assert!(
        result.is_ok(),
        "Commit with multiline message should succeed: {:?}",
        result.err()
    );

    // Verify the commit message body
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["log", "-1", "--format=%b"])
        .output()
        .expect("Failed to get git log");
    let body = String::from_utf8_lossy(&output.stdout);
    assert!(
        body.contains("This is the body of the commit message"),
        "Commit body should be preserved, got: {body}"
    );
}

/// Test 5: commit with unicode message should work
/// Verifies: unicode characters in commit messages are properly handled
#[test]
fn commit_with_unicode_message() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add initial.txt", temp_git.path());

    // Commit with unicode message
    let unicode_message = "fix: 日本語メッセージ 🎉 émoji";
    let result = git::commit(temp_git.path(), unicode_message);
    assert!(
        result.is_ok(),
        "Commit with unicode should succeed: {:?}",
        result.err()
    );

    // Verify the commit message
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["log", "-1", "--format=%s"])
        .output()
        .expect("Failed to get git log");
    let message = String::from_utf8_lossy(&output.stdout);
    assert!(
        message.trim() == unicode_message,
        "Commit message should match with unicode, got: {message}"
    );
}

/// Test 6: verify stdin piping works correctly
/// Verifies: stdin is properly piped to git commit (core functionality)
#[test]
fn commit_stdin_piping() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add initial.txt", temp_git.path());

    // Create a very long message to ensure stdin is used properly
    let long_message = format!("test: {}", "x".repeat(10000));
    let result = git::commit(temp_git.path(), &long_message);
    assert!(
        result.is_ok(),
        "Commit with long message via stdin should succeed: {:?}",
        result.err()
    );

    // Verify the commit was created
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["log", "-1", "--format=%s"])
        .output()
        .expect("Failed to get git log");
    let message = String::from_utf8_lossy(&output.stdout);
    assert!(
        message.starts_with("test: "),
        "Commit message should start with 'test: ', got: {message}"
    );
    assert!(
        message.len() > 10000,
        "Commit message should be preserved in full, length: {}",
        message.len()
    );
}

/// Test 7: commit multiple staged files
/// Verifies: successful commit handles multiple files
#[test]
fn commit_multiple_staged_files() {
    let temp_git = empty_git_repo();

    // Create initial commit with multiple files
    let file1 = temp_git.path().join("file1.txt");
    let file2 = temp_git.path().join("file2.txt");
    File::create(&file1)
        .unwrap()
        .write_all(b"content1")
        .unwrap();
    File::create(&file2)
        .unwrap()
        .write_all(b"content2")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Modify both files and stage them
    File::create(&file1)
        .unwrap()
        .write_all(b"modified1")
        .unwrap();
    File::create(&file2)
        .unwrap()
        .write_all(b"modified2")
        .unwrap();
    exec_git_cmd("add file1.txt file2.txt", temp_git.path());

    // Commit should succeed with both files
    let result = git::commit(temp_git.path(), "update both files");
    assert!(
        result.is_ok(),
        "Commit should succeed with multiple staged files: {:?}",
        result.err()
    );

    // Verify both files are committed
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["show", "--stat", "--format="])
        .output()
        .expect("Failed to get git show");
    let stat = String::from_utf8_lossy(&output.stdout);
    assert!(
        stat.contains("file1.txt"),
        "file1.txt should be in commit: {stat}"
    );
    assert!(
        stat.contains("file2.txt"),
        "file2.txt should be in commit: {stat}"
    );
}

/// Test 8: commit with newly added file (not just modification)
/// Verifies: new files can be committed
#[test]
fn commit_new_file() {
    let temp_git = empty_git_repo();

    // Create initial commit
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

    // Commit should succeed
    let result = git::commit(temp_git.path(), "add new file");
    assert!(
        result.is_ok(),
        "Commit should succeed with new file: {:?}",
        result.err()
    );

    // Verify the new file is in the commit
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["show", "--stat", "--format="])
        .output()
        .expect("Failed to get git show");
    let stat = String::from_utf8_lossy(&output.stdout);
    assert!(
        stat.contains("new_file.txt"),
        "new_file.txt should be in commit: {stat}"
    );
}

/// Test 9: commit with deleted file
/// Verifies: deleted files can be committed
#[test]
fn commit_deleted_file() {
    let temp_git = empty_git_repo();

    // Create initial commit with a file
    let file = temp_git.path().join("to_delete.txt");
    File::create(&file)
        .unwrap()
        .write_all(b"will be deleted")
        .unwrap();
    create_commit(temp_git.path(), true);

    // Delete and stage the deletion
    std::fs::remove_file(&file).unwrap();
    exec_git_cmd("add to_delete.txt", temp_git.path());

    // Commit should succeed
    let result = git::commit(temp_git.path(), "delete file");
    assert!(
        result.is_ok(),
        "Commit should succeed with deleted file: {:?}",
        result.err()
    );

    // Verify the file is deleted in the commit
    let output = Command::new("git")
        .arg("-C")
        .arg(temp_git.path())
        .args(["show", "--stat", "--format="])
        .output()
        .expect("Failed to get git show");
    let stat = String::from_utf8_lossy(&output.stdout);
    assert!(
        stat.contains("to_delete.txt"),
        "to_delete.txt should be in commit as deleted: {stat}"
    );
}

/// Test 10: commit with empty message content (whitespace only)
/// Verifies: empty commit message shows appropriate error
#[test]
fn commit_whitespace_message() {
    let temp_git = empty_git_repo();

    // Create initial commit
    let file = temp_git.path().join("initial.txt");
    File::create(&file).unwrap().write_all(b"initial").unwrap();
    create_commit(temp_git.path(), true);

    // Modify and stage file
    File::create(&file).unwrap().write_all(b"modified").unwrap();
    exec_git_cmd("add initial.txt", temp_git.path());

    // Commit with whitespace-only message should fail
    let result = git::commit(temp_git.path(), "   \n\t  ");
    assert!(
        result.is_err(),
        "Commit with whitespace-only message should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("empty commit message") || err.contains("Aborting commit"),
        "Error should indicate empty message, got: {err}"
    );
}
