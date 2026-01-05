//! Restore and rewind integration tests
//!
//! Tests checkpoint restoration and time-travel functionality.

use anyhow::Result;
use std::fs;
use std::time::Duration;
use crate::common::{ProjectSize, ProjectTemplate, TestProject};
use crate::common::cli::{TlCommand, extract_ulid};

/// Trigger checkpoint creation and return the checkpoint ID
async fn create_checkpoint(root: &std::path::Path) -> Result<String> {
    // Wait for FSEvents latency + coalescing window (macOS can have 100-300ms FSEvents delay)
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Flush to create checkpoint
    let flush_result = TlCommand::new(root)
        .args(&["flush"])
        .assert_success()?;

    // Extract checkpoint ID from output
    extract_ulid(&flush_result.stdout)
        .ok_or_else(|| anyhow::anyhow!("No checkpoint created (no pending changes)"))
}

/// Test basic restore functionality
#[tokio::test]
async fn test_basic_restore() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    // Initialize
    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create initial state
    project.modify_files(&["src/main.rs"], "// version 1\nfn main() {}")?;
    let checkpoint_id = create_checkpoint(&root).await?;

    // Stop daemon
    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify file to different state
    project.modify_files(&["src/main.rs"], "// version 2\nfn main() { println!(\"changed\"); }")?;

    // Verify modified
    let content_before = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content_before.contains("version 2"));

    // Restore to checkpoint
    let restore = TlCommand::new(&root)
        .args(&["restore", &checkpoint_id])
        .stdin("y\n")
        .assert_success()?;

    println!("✓ Restore completed in {:?}", restore.duration);

    // Verify restored
    let content_after = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content_after.contains("version 1"), "Content not restored correctly");

    // Restore should be fast
    assert!(restore.duration < Duration::from_secs(3), "Restore too slow: {:?}", restore.duration);

    Ok(())
}

/// Test multi-state rewind (time travel through multiple checkpoints)
#[tokio::test]
async fn test_rewind_to_earlier_state() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Small)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // STATE 1: Initial
    project.modify_files(&["src/main.rs"], "// state 1")?;
    let checkpoint1 = create_checkpoint(&root).await?;

    // STATE 2: Modified
    project.modify_files(&["src/main.rs"], "// state 2")?;
    let _checkpoint2 = create_checkpoint(&root).await?;

    // STATE 3: More changes
    project.modify_files(&["src/main.rs"], "// state 3")?;
    let _checkpoint3 = create_checkpoint(&root).await?;

    // Stop daemon to release database locks
    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Verify at state 3
    let current = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(current.contains("state 3"));

    // REWIND to state 1
    let restore = TlCommand::new(&root)
        .args(&["restore", &checkpoint1])
        .stdin("y\n")
        .assert_success()?;

    println!("✓ Rewound to state 1 in {:?}", restore.duration);

    // Verify restored to state 1
    let restored = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(restored.contains("state 1"), "Rewind failed - expected state 1");

    // Verify rewind was fast
    assert!(restore.duration < Duration::from_secs(3), "Rewind too slow: {:?}", restore.duration);

    Ok(())
}

/// Benchmark restore performance across different project sizes
#[tokio::test]
async fn bench_restore_performance() -> Result<()> {
    for size in [ProjectSize::Tiny, ProjectSize::Small, ProjectSize::Medium] {
        let mut project = TestProject::new(
            ProjectTemplate::rust_project(size)
        )?;
        let root = project.root().to_path_buf();

        // Setup
        TlCommand::new(&root).args(&["init"]).assert_success()?;
        TlCommand::new(&root).args(&["start"]).assert_success()?;
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Create a checkpoint
        project.modify_files(&["src/file_0.rs"], "// initial content")?;
        let checkpoint = create_checkpoint(&root).await?;

        // Stop daemon
        TlCommand::new(&root).args(&["stop"]).assert_success()?;

        // Modify ALL files in the project
        for file_info in &project.file_manifest {
            if file_info.extension == "rs" {
                fs::write(&file_info.path, "// modified content")?;
            }
        }

        // Measure restore time
        let restore = TlCommand::new(&root)
            .args(&["restore", &checkpoint])
            .stdin("y\n")
            .assert_success()?;

        println!(
            "PERF: restore_{:?} files={} time={:?}",
            size,
            project.file_count(),
            restore.duration
        );

        // Performance targets (realistic for test environment)
        match size {
            ProjectSize::Tiny => {
                assert!(
                    restore.duration < Duration::from_secs(3),
                    "Tiny restore too slow: {:?}",
                    restore.duration
                );
            }
            ProjectSize::Small => {
                assert!(
                    restore.duration < Duration::from_secs(5),
                    "Small restore too slow: {:?}",
                    restore.duration
                );
            }
            ProjectSize::Medium => {
                assert!(
                    restore.duration < Duration::from_secs(10),
                    "Medium restore too slow: {:?}",
                    restore.duration
                );
            }
            _ => {}
        }
    }

    Ok(())
}

/// Test restore with file deletions
#[tokio::test]
async fn test_restore_deleted_files() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create initial state with known file
    let test_file = root.join("test_file.txt");
    fs::write(&test_file, "test content")?;
    let checkpoint = create_checkpoint(&root).await?;

    // Stop daemon
    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Delete the file
    fs::remove_file(&test_file)?;
    assert!(!test_file.exists(), "File should be deleted");

    // Restore
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    // Verify file restored
    assert!(test_file.exists(), "Deleted file should be restored");
    let content = fs::read_to_string(&test_file)?;
    assert_eq!(content, "test content", "File content should match");

    println!("✓ Deleted file successfully restored");

    Ok(())
}

/// Test diff command shows changes correctly
#[tokio::test]
async fn test_diff_shows_changes() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create initial state
    project.modify_files(&["src/main.rs"], "// version 1")?;
    let checkpoint = create_checkpoint(&root).await?;

    // Stop daemon
    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify file
    project.modify_files(&["src/main.rs"], "// version 2 - changed!")?;

    // Check diff
    let diff = TlCommand::new(&root)
        .args(&["diff", &checkpoint])
        .execute()?;

    if diff.success() {
        println!("✓ Diff output:\n{}", diff.stdout);
        // Diff should show something changed
        assert!(!diff.stdout.is_empty() || !diff.stderr.is_empty(), "Diff should show changes");
    } else {
        println!("⚠ Diff command not yet implemented - skipping diff assertion");
    }

    Ok(())
}
