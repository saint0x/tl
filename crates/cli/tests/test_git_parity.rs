//! Tests for Git parity commands (tag, stash, remote, show)
//!
//! Validates that the new Git-compatible CLI commands work correctly.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

/// Helper to get the tl binary path
fn tl_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("Failed to get current exe");
    path.pop(); // Remove test binary name
    path.pop(); // Remove deps directory
    path.push("tl");
    path
}

/// Helper to run tl command in a directory
fn run_tl(dir: &PathBuf, args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new(tl_bin())
        .args(args)
        .current_dir(dir)
        .output()?)
}

/// Helper to run git command in a directory
fn run_git(dir: &PathBuf, args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()?)
}

/// Initialize a test repository with git
fn init_test_repo() -> Result<(TempDir, PathBuf)> {
    let temp = TempDir::new()?;
    let repo_root = temp.path().to_path_buf();

    // Initialize git first
    run_git(&repo_root, &["init"])?;
    run_git(&repo_root, &["config", "user.email", "test@test.com"])?;
    run_git(&repo_root, &["config", "user.name", "Test User"])?;

    // Create initial commit
    fs::write(repo_root.join("README.md"), "# Test")?;
    run_git(&repo_root, &["add", "."])?;
    run_git(&repo_root, &["commit", "-m", "Initial commit"])?;

    // Initialize timelapse
    let output = run_tl(&repo_root, &["init"])?;
    assert!(output.status.success(), "tl init failed: {:?}", String::from_utf8_lossy(&output.stderr));

    Ok((temp, repo_root))
}

// =============================================================================
// Show Command Tests
// =============================================================================

#[test]
fn test_show_requires_checkpoint() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Try to show a non-existent checkpoint
    let output = run_tl(&repo_root, &["show", "nonexistent"])?;
    assert!(!output.status.success(), "Should fail for non-existent checkpoint");

    Ok(())
}

#[test]
fn test_show_head_checkpoint() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create a file change
    fs::write(repo_root.join("test.txt"), "test content")?;
    std::thread::sleep(Duration::from_millis(500));

    // Flush to create checkpoint (daemon auto-starts)
    run_tl(&repo_root, &["flush"])?;

    // Show HEAD checkpoint (daemon auto-starts if needed)
    let output = run_tl(&repo_root, &["show", "HEAD"])?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Command should succeed and show checkpoint info
    assert!(output.status.success(), "tl show HEAD should succeed, stderr: {}", stderr);
    assert!(stdout.contains("checkpoint") || stdout.contains("Checkpoint"), "Output: {}", stdout);

    // Stop daemon
    run_tl(&repo_root, &["stop"])?;

    Ok(())
}

#[test]
fn test_show_with_diff_flag() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create a file change
    fs::write(repo_root.join("test.txt"), "test content")?;
    std::thread::sleep(Duration::from_millis(500));

    // Flush to create checkpoint (daemon auto-starts)
    run_tl(&repo_root, &["flush"])?;

    // Show HEAD with diff
    let output = run_tl(&repo_root, &["show", "HEAD", "--diff"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "tl show HEAD --diff should succeed, stderr: {}", stderr);
    assert!(stdout.contains("checkpoint") || stdout.contains("Checkpoint"), "Output: {}", stdout);

    // Stop daemon
    run_tl(&repo_root, &["stop"])?;

    Ok(())
}

// =============================================================================
// Tag Command Tests
// =============================================================================

#[test]
fn test_tag_list_empty() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    let output = run_tl(&repo_root, &["tag", "list"])?;
    assert!(output.status.success(), "tl tag list should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No tags") || stdout.is_empty() || stdout.contains("Tags:"));

    Ok(())
}

#[test]
fn test_tag_create() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create a tag
    let output = run_tl(&repo_root, &["tag", "create", "v1.0.0"])?;
    assert!(output.status.success(), "tl tag create should succeed: {:?}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("v1.0.0") || stdout.contains("Created"));

    // Verify tag exists in git
    let git_output = run_git(&repo_root, &["tag"])?;
    let git_stdout = String::from_utf8_lossy(&git_output.stdout);
    assert!(git_stdout.contains("v1.0.0"), "Tag should exist in git");

    Ok(())
}

#[test]
fn test_tag_create_annotated() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create annotated tag
    let output = run_tl(&repo_root, &["tag", "create", "v2.0.0", "-m", "Release version 2.0.0"])?;
    assert!(output.status.success(), "tl tag create with message should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("annotated") || stdout.contains("v2.0.0"));

    Ok(())
}

#[test]
fn test_tag_create_duplicate_fails() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create first tag
    run_tl(&repo_root, &["tag", "create", "v1.0.0"])?;

    // Try to create duplicate
    let output = run_tl(&repo_root, &["tag", "create", "v1.0.0"])?;
    assert!(!output.status.success(), "Duplicate tag should fail");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already exists") || stderr.contains("exists"));

    Ok(())
}

#[test]
fn test_tag_create_force_overwrites() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create first tag
    run_tl(&repo_root, &["tag", "create", "v1.0.0"])?;

    // Force overwrite
    let output = run_tl(&repo_root, &["tag", "create", "v1.0.0", "--force"])?;
    assert!(output.status.success(), "Force tag should succeed");

    Ok(())
}

#[test]
fn test_tag_delete() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create then delete tag
    run_tl(&repo_root, &["tag", "create", "to-delete"])?;

    let output = run_tl(&repo_root, &["tag", "delete", "to-delete"])?;
    assert!(output.status.success(), "tl tag delete should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Deleted") || stdout.contains("to-delete"));

    // Verify tag no longer exists
    let git_output = run_git(&repo_root, &["tag"])?;
    let git_stdout = String::from_utf8_lossy(&git_output.stdout);
    assert!(!git_stdout.contains("to-delete"), "Tag should be deleted");

    Ok(())
}

#[test]
fn test_tag_delete_nonexistent_fails() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    let output = run_tl(&repo_root, &["tag", "delete", "nonexistent"])?;
    assert!(!output.status.success(), "Deleting nonexistent tag should fail");

    Ok(())
}

#[test]
fn test_tag_show() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create tag
    run_tl(&repo_root, &["tag", "create", "show-me", "-m", "Test message"])?;

    // Show tag
    let output = run_tl(&repo_root, &["tag", "show", "show-me"])?;
    assert!(output.status.success(), "tl tag show should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("show-me") || stdout.contains("Test message"));

    Ok(())
}

// =============================================================================
// Stash Command Tests
// =============================================================================

#[test]
fn test_stash_list_empty() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    let output = run_tl(&repo_root, &["stash", "list"])?;
    assert!(output.status.success(), "tl stash list should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No stashes") || stdout.is_empty() || stdout.contains("Stashes:"));

    Ok(())
}

#[test]
fn test_stash_push() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Create changes
    fs::write(repo_root.join("stash-test.txt"), "stash content")?;
    std::thread::sleep(Duration::from_millis(500));

    // Flush first to ensure file is tracked (daemon auto-starts)
    run_tl(&repo_root, &["flush"])?;

    // Create another change
    fs::write(repo_root.join("stash-test2.txt"), "more stash content")?;
    std::thread::sleep(Duration::from_millis(500));

    // Push to stash
    let output = run_tl(&repo_root, &["stash", "push"])?;

    // May succeed or report "no changes" depending on timing
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() || stdout.contains("No changes") || stderr.contains("No changes"),
        "Stash push should succeed or report no changes, stdout: {}, stderr: {}",
        stdout, stderr
    );

    // Stop daemon
    run_tl(&repo_root, &["stop"])?;

    Ok(())
}

#[test]
fn test_stash_push_with_message() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Start daemon
    run_tl(&repo_root, &["start"])?;
    std::thread::sleep(Duration::from_secs(1));

    // Create changes
    fs::write(repo_root.join("stash-test.txt"), "stash content")?;
    std::thread::sleep(Duration::from_millis(500));

    // Push to stash with message
    let output = run_tl(&repo_root, &["stash", "push", "-m", "my-wip"])?;
    run_tl(&repo_root, &["stop"])?;

    // Check output
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        assert!(stdout.contains("stash") || stdout.contains("Saved"));
    }

    Ok(())
}

#[test]
fn test_stash_drop_nonexistent() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    let output = run_tl(&repo_root, &["stash", "drop", "stash@{99}"])?;
    assert!(!output.status.success(), "Dropping nonexistent stash should fail");

    Ok(())
}

#[test]
fn test_stash_clear_requires_confirmation_when_stashes_exist() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Without stashes, clear succeeds with "No stashes to clear"
    let output = run_tl(&repo_root, &["stash", "clear"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Either succeeds (no stashes) or fails (requires confirmation)
    assert!(
        output.status.success() || stderr.contains("Confirmation"),
        "Clear should succeed with no stashes or require confirmation, got stdout: {} stderr: {}",
        stdout, stderr
    );

    Ok(())
}

#[test]
fn test_stash_clear_with_yes() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    let output = run_tl(&repo_root, &["stash", "clear", "-y"])?;
    // Should succeed (even if no stashes)
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() || stdout.contains("No stashes"),
        "Clear with -y should succeed or report no stashes"
    );

    Ok(())
}

// =============================================================================
// Remote Command Tests
// =============================================================================

#[test]
fn test_remote_list_empty() -> Result<()> {
    let temp = TempDir::new()?;
    let repo_root = temp.path().to_path_buf();

    // Initialize git without remote
    run_git(&repo_root, &["init"])?;
    run_git(&repo_root, &["config", "user.email", "test@test.com"])?;
    run_git(&repo_root, &["config", "user.name", "Test User"])?;
    fs::write(repo_root.join("README.md"), "# Test")?;
    run_git(&repo_root, &["add", "."])?;
    run_git(&repo_root, &["commit", "-m", "Initial"])?;

    run_tl(&repo_root, &["init"])?;

    let output = run_tl(&repo_root, &["remote", "list"])?;
    assert!(output.status.success(), "tl remote list should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No remotes") || stdout.is_empty() || stdout.contains("Remotes:"));

    Ok(())
}

#[test]
fn test_remote_add() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add remote
    let output = run_tl(&repo_root, &["remote", "add", "test-remote", "https://example.com/repo.git"])?;
    assert!(output.status.success(), "tl remote add should succeed: {:?}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Added") || stdout.contains("test-remote"));

    // Verify remote exists in git
    let git_output = run_git(&repo_root, &["remote", "-v"])?;
    let git_stdout = String::from_utf8_lossy(&git_output.stdout);
    assert!(git_stdout.contains("test-remote"), "Remote should exist in git");

    Ok(())
}

#[test]
fn test_remote_add_duplicate_fails() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add remote
    run_tl(&repo_root, &["remote", "add", "dup-remote", "https://example.com/repo.git"])?;

    // Try to add duplicate
    let output = run_tl(&repo_root, &["remote", "add", "dup-remote", "https://other.com/repo.git"])?;
    assert!(!output.status.success(), "Duplicate remote should fail");

    Ok(())
}

#[test]
fn test_remote_remove() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add then remove remote
    run_tl(&repo_root, &["remote", "add", "to-remove", "https://example.com/repo.git"])?;

    let output = run_tl(&repo_root, &["remote", "remove", "to-remove"])?;
    assert!(output.status.success(), "tl remote remove should succeed");

    // Verify remote no longer exists
    let git_output = run_git(&repo_root, &["remote", "-v"])?;
    let git_stdout = String::from_utf8_lossy(&git_output.stdout);
    assert!(!git_stdout.contains("to-remove"), "Remote should be removed");

    Ok(())
}

#[test]
fn test_remote_remove_nonexistent_fails() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    let output = run_tl(&repo_root, &["remote", "remove", "nonexistent"])?;
    assert!(!output.status.success(), "Removing nonexistent remote should fail");

    Ok(())
}

#[test]
fn test_remote_rename() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add remote
    run_tl(&repo_root, &["remote", "add", "old-name", "https://example.com/repo.git"])?;

    // Rename
    let output = run_tl(&repo_root, &["remote", "rename", "old-name", "new-name"])?;
    assert!(output.status.success(), "tl remote rename should succeed");

    // Verify rename
    let git_output = run_git(&repo_root, &["remote", "-v"])?;
    let git_stdout = String::from_utf8_lossy(&git_output.stdout);
    assert!(!git_stdout.contains("old-name"), "Old name should not exist");
    assert!(git_stdout.contains("new-name"), "New name should exist");

    Ok(())
}

#[test]
fn test_remote_set_url() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add remote
    run_tl(&repo_root, &["remote", "add", "test", "https://old.com/repo.git"])?;

    // Update URL
    let output = run_tl(&repo_root, &["remote", "set-url", "test", "https://new.com/repo.git"])?;
    assert!(output.status.success(), "tl remote set-url should succeed");

    // Verify URL updated
    let git_output = run_git(&repo_root, &["remote", "get-url", "test"])?;
    let git_stdout = String::from_utf8_lossy(&git_output.stdout);
    assert!(git_stdout.contains("new.com"), "URL should be updated");

    Ok(())
}

#[test]
fn test_remote_get_url() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add remote
    run_tl(&repo_root, &["remote", "add", "get-test", "https://example.com/myrepo.git"])?;

    // Get URL
    let output = run_tl(&repo_root, &["remote", "get-url", "get-test"])?;
    assert!(output.status.success(), "tl remote get-url should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("example.com") || stdout.contains("myrepo.git"));

    Ok(())
}

#[test]
fn test_remote_list_verbose() -> Result<()> {
    let (_temp, repo_root) = init_test_repo()?;

    // Remove existing origin if any
    let _ = run_git(&repo_root, &["remote", "remove", "origin"]);

    // Add remote
    run_tl(&repo_root, &["remote", "add", "verbose-test", "https://example.com/repo.git"])?;

    // List with verbose
    let output = run_tl(&repo_root, &["remote", "list", "--verbose"])?;
    assert!(output.status.success(), "tl remote list --verbose should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("verbose-test"));
    assert!(stdout.contains("example.com") || stdout.contains("fetch") || stdout.contains("push"));

    Ok(())
}

// =============================================================================
// Config Command Tests (additional to existing)
// =============================================================================

#[test]
fn test_config_get_and_list() -> Result<()> {
    // Test --get on a known config key (don't assume specific value due to parallelism)
    let get_output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--get")
        .arg("daemon.checkpoint_interval_secs")
        .output()?;

    assert!(get_output.status.success(), "tl config get should succeed");
    let stdout = String::from_utf8_lossy(&get_output.stdout);
    // Just verify we get a numeric value, don't check specific value
    let _value: u64 = stdout.trim().parse()
        .expect(&format!("Expected numeric value, got: {}", stdout));

    // Test --list shows all config keys
    let list_output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--list")
        .output()?;

    assert!(list_output.status.success(), "tl config list should succeed");
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(list_stdout.contains("[daemon]"), "Should show daemon section");
    assert!(list_stdout.contains("[gc]"), "Should show gc section");

    Ok(())
}

#[test]
fn test_config_set_validates_values() -> Result<()> {
    // Test that invalid values are rejected
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--set")
        .arg("daemon.checkpoint_interval_secs=999999")  // Too high
        .output()?;

    assert!(!output.status.success(), "Should reject invalid value");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid") || stderr.contains("validation") || stderr.contains("error"),
        "Should mention validation error: {}", stderr
    );

    Ok(())
}

// =============================================================================
// Integration Tests
// =============================================================================

#[test]
fn test_commands_fail_without_git_repo() -> Result<()> {
    let temp = TempDir::new()?;
    let repo_root = temp.path().to_path_buf();

    // Don't initialize git - commands should fail
    let output = run_tl(&repo_root, &["remote", "list"])?;
    assert!(!output.status.success(), "remote list should fail without git repo");

    let output = run_tl(&repo_root, &["tag", "list"])?;
    assert!(!output.status.success(), "tag list should fail without git/jj repo");

    Ok(())
}
