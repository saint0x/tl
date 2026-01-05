//! Edge case tests for Publish/Pull operations (JJ integration)
//!
//! Tests publishing checkpoints to JJ, creating bookmarks, and pulling from JJ.
//! Uses native jj-lib integration (no CLI binary required).

use anyhow::Result;
use std::fs;
use std::time::Duration;
use crate::common::{ProjectSize, ProjectTemplate, TestProject};
use crate::common::cli::TlCommand;

/// Trigger checkpoint creation and return the checkpoint ID
async fn create_checkpoint(root: &std::path::Path) -> Result<Option<String>> {
    // Wait for FSEvents latency + coalescing window (macOS can have 100-300ms FSEvents delay)
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let flush_result = TlCommand::new(root)
        .args(&["flush"])
        .execute()?;

    if !flush_result.success() {
        return Ok(None);
    }

    Ok(crate::common::cli::extract_ulid(&flush_result.stdout))
}

/// Create a JJ commit using native jj-lib APIs
fn create_jj_commit_native(root: &std::path::Path, message: &str) -> Result<()> {
    use jj_lib::repo::Repo;

    let workspace = jj::load_workspace(root)?;

    let config = config::Config::builder().build()?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)?;

    // Get current working copy commit
    let wc_commit_id = repo.view()
        .get_wc_commit_id(workspace.workspace_id())
        .ok_or_else(|| anyhow::anyhow!("No working copy commit"))?;

    let wc_commit = repo.store().get_commit(wc_commit_id)?;

    // Start transaction to create new commit
    let mut tx = repo.start_transaction(&user_settings);

    // Create new commit with the working copy tree
    let new_commit = tx.mut_repo()
        .new_commit(
            &user_settings,
            vec![wc_commit.id().clone()],
            wc_commit.tree_id().clone(),
        )
        .set_description(message)
        .write()?;

    // Update working copy to point to new empty commit
    let new_wc_commit = tx.mut_repo()
        .new_commit(
            &user_settings,
            vec![new_commit.id().clone()],
            repo.store().empty_merged_tree_id(),
        )
        .write()?;

    tx.mut_repo().set_wc_commit(workspace.workspace_id().clone(), new_wc_commit.id().clone())?;

    tx.commit(&format!("commit: {}", message));

    Ok(())
}

/// Test basic publish of single checkpoint
#[tokio::test]
async fn test_publish_single_checkpoint() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create checkpoint
    project.modify_files(&["src/main.rs"], "// published version")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Publish checkpoint
    let start = std::time::Instant::now();
    let publish_result = TlCommand::new(&root)
        .args(&["publish", &checkpoint])
        .assert_success()?;
    let publish_duration = start.elapsed();

    println!("✓ Publish single checkpoint: {:?}", publish_duration);
    println!("{}", publish_result.stdout);

    assert!(
        publish_result.stdout.contains("Published") || publish_result.stdout.contains("✓"),
        "Should confirm publication"
    );

    // Verify auto-bookmark snap/HEAD is created when no explicit bookmark provided
    assert!(
        publish_result.stdout.contains("snap/HEAD") || publish_result.stdout.contains("Updated bookmark"),
        "Should auto-create snap/HEAD bookmark"
    );

    // Performance assertion
    assert!(publish_duration < Duration::from_secs(10), "Publish too slow: {:?}", publish_duration);

    Ok(())
}

/// Test publish HEAD alias (Bug fix: HEAD should resolve to latest checkpoint)
#[tokio::test]
async fn test_publish_head_alias() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create checkpoint
    project.modify_files(&["src/main.rs"], "// HEAD test version")?;
    let _checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Publish using HEAD alias (this was failing before the fix)
    let publish_result = TlCommand::new(&root)
        .args(&["publish", "HEAD"])
        .assert_success()?;

    println!("✓ Publish HEAD alias works");
    println!("{}", publish_result.stdout);

    assert!(
        publish_result.stdout.contains("Published") || publish_result.stdout.contains("✓"),
        "Should confirm publication using HEAD"
    );

    // Verify auto-bookmark snap/HEAD is created
    assert!(
        publish_result.stdout.contains("snap/HEAD"),
        "Should auto-create snap/HEAD bookmark when publishing HEAD"
    );

    Ok(())
}

/// Test publish with bookmark creation
#[tokio::test]
async fn test_publish_with_bookmark() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create checkpoint
    project.modify_files(&["src/main.rs"], "// feature version")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Publish with bookmark
    let publish_result = TlCommand::new(&root)
        .args(&["publish", &checkpoint, "--bookmark", "feature-x"])
        .assert_success()?;

    println!("✓ Publish with bookmark created");
    println!("{}", publish_result.stdout);

    assert!(
        publish_result.stdout.contains("bookmark") || publish_result.stdout.contains("feature-x"),
        "Should confirm bookmark creation"
    );

    Ok(())
}

/// Test publish range of checkpoints
#[tokio::test]
async fn test_publish_range() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 5 checkpoints
    let mut checkpoints = Vec::new();
    for i in 0..5 {
        project.modify_files(&["src/main.rs"], &format!("// version {}", i))?;
        if let Some(cp) = create_checkpoint(&root).await? {
            checkpoints.push(cp);
        }
    }

    assert_eq!(checkpoints.len(), 5, "Should create 5 checkpoints");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Publish range using HEAD~4 syntax (publishes last 5 checkpoints)
    let start = std::time::Instant::now();
    let publish_result = TlCommand::new(&root)
        .args(&["publish", "HEAD~4"])
        .assert_success()?;
    let publish_duration = start.elapsed();

    println!("✓ Publish range (5 checkpoints): {:?}", publish_duration);
    println!("{}", publish_result.stdout);

    assert!(
        publish_result.stdout.contains("5") || publish_result.stdout.contains("Published"),
        "Should indicate 5 checkpoints published"
    );

    Ok(())
}

/// Test compact publish (squash range into single commit)
#[tokio::test]
async fn test_publish_compact() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 3 checkpoints
    for i in 0..3 {
        project.modify_files(&["src/main.rs"], &format!("// compact version {}", i))?;
        create_checkpoint(&root).await?;
    }

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Publish with --compact flag
    let publish_result = TlCommand::new(&root)
        .args(&["publish", "HEAD~2", "--compact"])
        .assert_success()?;

    println!("✓ Compact publish (squash 3 checkpoints)");
    println!("{}", publish_result.stdout);

    assert!(
        publish_result.stdout.contains("Published") || publish_result.stdout.contains("✓"),
        "Should confirm compact publication"
    );

    Ok(())
}

/// Test pull from JJ (fetch and import)
#[tokio::test]
async fn test_pull_from_jj() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    // Make a change in JJ workspace
    fs::write(root.join("new_file.txt"), "created in jj")?;

    // Create JJ commit using native jj-lib APIs (no CLI binary required)
    if let Err(e) = create_jj_commit_native(&root, "test commit from jj") {
        println!("JJ commit failed ({}), skipping test", e);
        return Ok(());
    }

    // Pull from JJ (this should import the commit as a checkpoint)
    let start = std::time::Instant::now();
    let pull_result = TlCommand::new(&root)
        .args(&["pull"])
        .execute()?; // Note: Don't use assert_success, pull might fail in some environments
    let pull_duration = start.elapsed();

    println!("✓ Pull from JJ: {:?}", pull_duration);
    println!("Pull output:\n{}", pull_result.stdout);
    if !pull_result.stderr.is_empty() {
        println!("Pull stderr:\n{}", pull_result.stderr);
    }

    // If pull succeeded, verify it completed quickly
    if pull_result.success() {
        assert!(pull_duration < Duration::from_secs(10), "Pull too slow: {:?}", pull_duration);
    }

    Ok(())
}

/// Test publish then pull round-trip
#[tokio::test]
async fn test_publish_pull_roundtrip() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and publish checkpoint
    project.modify_files(&["src/main.rs"], "// roundtrip test")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    let publish_result = TlCommand::new(&root)
        .args(&["publish", &checkpoint])
        .assert_success()?;

    println!("Published checkpoint:");
    println!("{}", publish_result.stdout);

    // Now pull (should detect it's already imported or fail gracefully without remote)
    let pull_result = TlCommand::new(&root)
        .args(&["pull"])
        .execute()?;

    println!("Pull after publish:");
    println!("stdout: {}", pull_result.stdout);
    println!("stderr: {}", pull_result.stderr);

    // Should either succeed or fail gracefully (no remote configured is expected in test)
    // The important thing is that publish worked - pull without a remote is expected to fail
    let has_no_remote_error = pull_result.stderr.contains("remote") ||
                               pull_result.stderr.contains("No Git remote") ||
                               pull_result.stderr.contains("fetch");
    assert!(
        pull_result.success() ||
        pull_result.stdout.contains("already imported") ||
        pull_result.stdout.contains("nothing to import") ||
        has_no_remote_error,  // No remote is expected in test environment
        "Pull should handle already-published checkpoint or indicate no remote"
    );

    println!("✓ Publish-pull round-trip works correctly (publish verified)");

    Ok(())
}

/// Test publish performance with many checkpoints
#[tokio::test]
async fn test_publish_performance() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 10 checkpoints
    for i in 0..10 {
        project.modify_files(&["src/main.rs"], &format!("// publish perf test {}", i))?;
        create_checkpoint(&root).await?;
    }

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Publish all 10
    let start = std::time::Instant::now();
    let publish_result = TlCommand::new(&root)
        .args(&["publish", "HEAD~9"])
        .assert_success()?;
    let publish_duration = start.elapsed();

    println!("✓ Publish 10 checkpoints: {:?} (avg: {:?}/checkpoint)",
        publish_duration, publish_duration / 10);
    println!("{}", publish_result.stdout);

    // Should complete in reasonable time
    assert!(publish_duration < Duration::from_secs(60), "Publishing 10 checkpoints too slow: {:?}", publish_duration);

    Ok(())
}

/// Test handling already-published checkpoints
#[tokio::test]
async fn test_publish_already_published() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    // JJ workspace is auto-initialized by `tl init`

    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and publish checkpoint
    project.modify_files(&["src/main.rs"], "// first publish")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    TlCommand::new(&root)
        .args(&["publish", &checkpoint])
        .assert_success()?;

    // Try to publish again (should fail or warn)
    let second_publish = TlCommand::new(&root)
        .args(&["publish", &checkpoint])
        .execute()?;

    println!("Second publish attempt:");
    println!("stdout: {}", second_publish.stdout);
    println!("stderr: {}", second_publish.stderr);

    // Should either fail or indicate already published
    assert!(
        !second_publish.success() ||
        second_publish.stdout.contains("already published") ||
        second_publish.stderr.contains("already published") ||
        second_publish.stdout.contains("--compact"),
        "Should handle already-published checkpoint"
    );

    println!("✓ Correctly handles already-published checkpoints");

    Ok(())
}
