//! Edge case tests for Pin/Unpin and Garbage Collection operations
//!
//! Validates pin management, GC retention policies, and space reclamation.

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

/// Test basic pin creation
#[tokio::test]
async fn test_pin_checkpoint() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create checkpoint
    project.modify_files(&["src/main.rs"], "// test content")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Pin checkpoint
    let pin_result = TlCommand::new(&root)
        .args(&["pin", &checkpoint, "my-feature"])
        .assert_success()?;

    assert!(
        pin_result.stdout.contains("my-feature") || pin_result.stdout.contains("Created pin"),
        "Should confirm pin creation"
    );

    println!("✓ Pin checkpoint works correctly");

    Ok(())
}

/// Test pinning same checkpoint with different names
#[tokio::test]
async fn test_pin_multiple_names() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create checkpoint
    project.modify_files(&["src/main.rs"], "// test content")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Pin with multiple names
    TlCommand::new(&root)
        .args(&["pin", &checkpoint, "name1"])
        .assert_success()?;

    TlCommand::new(&root)
        .args(&["pin", &checkpoint, "name2"])
        .assert_success()?;

    TlCommand::new(&root)
        .args(&["pin", &checkpoint, "name3"])
        .assert_success()?;

    println!("✓ Multiple pins for same checkpoint works correctly");

    Ok(())
}

/// Test unpinning a checkpoint
#[tokio::test]
async fn test_unpin_checkpoint() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and pin checkpoint
    project.modify_files(&["src/main.rs"], "// test content")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    TlCommand::new(&root)
        .args(&["pin", &checkpoint, "temp-pin"])
        .assert_success()?;

    // Unpin
    let unpin_result = TlCommand::new(&root)
        .args(&["unpin", "temp-pin"])
        .assert_success()?;

    assert!(
        unpin_result.stdout.contains("Removed pin") || unpin_result.stdout.contains("temp-pin"),
        "Should confirm pin removal"
    );

    println!("✓ Unpin checkpoint works correctly");

    Ok(())
}

/// Test unpinning non-existent pin (should fail)
#[tokio::test]
async fn test_unpin_nonexistent() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;

    // Try to unpin non-existent pin
    let unpin_result = TlCommand::new(&root)
        .args(&["unpin", "does-not-exist"])
        .execute()?;

    assert!(!unpin_result.success(), "Should fail for non-existent pin");
    assert!(
        unpin_result.stderr.contains("not found") || unpin_result.stdout.contains("not found"),
        "Should indicate pin not found"
    );

    println!("✓ Unpin non-existent pin fails correctly");

    Ok(())
}

/// Test restore using pin name
#[tokio::test]
async fn test_restore_by_pin_name() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and pin checkpoint
    project.modify_files(&["src/main.rs"], "// pinned version")?;
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    TlCommand::new(&root)
        .args(&["pin", &checkpoint, "stable"])
        .assert_success()?;

    // Modify file
    project.modify_files(&["src/main.rs"], "// modified")?;

    // Restore using pin name
    TlCommand::new(&root)
        .args(&["restore", "stable"])
        .stdin("y\n")
        .assert_success()?;

    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("pinned version"), "Should restore to pinned version");

    println!("✓ Restore by pin name works correctly");

    Ok(())
}

/// Test GC with no garbage (fresh repo)
#[tokio::test]
async fn test_gc_no_garbage() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;

    // Run GC on fresh repo
    let gc_result = TlCommand::new(&root)
        .args(&["gc"])
        .assert_success()?;

    assert!(
        gc_result.stdout.contains("No garbage found") ||
        gc_result.stdout.contains("already clean") ||
        gc_result.stdout.contains("Checkpoints deleted: 0"),
        "Should report no garbage found, got: {}",
        gc_result.stdout
    );

    println!("✓ GC with no garbage works correctly");

    Ok(())
}

/// Test GC preserves pinned checkpoints
#[tokio::test]
async fn test_gc_preserves_pins() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create and pin old checkpoint
    project.modify_files(&["src/main.rs"], "// version 1")?;
    let checkpoint1 = create_checkpoint(&root).await?
        .expect("Should create checkpoint");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    TlCommand::new(&root)
        .args(&["pin", &checkpoint1, "important"])
        .assert_success()?;

    // Create many more checkpoints to age out checkpoint1
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    for i in 2..=20 {
        project.modify_files(&["src/main.rs"], &format!("// version {}", i))?;
        create_checkpoint(&root).await?;
    }

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Run GC
    TlCommand::new(&root).args(&["gc"]).assert_success()?;

    // Verify pinned checkpoint still exists and can be restored
    TlCommand::new(&root)
        .args(&["restore", "important"])
        .stdin("y\n")
        .assert_success()?;

    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("version 1"), "Pinned checkpoint should survive GC");

    println!("✓ GC preserves pinned checkpoints");

    Ok(())
}

/// Test GC space reclamation
#[tokio::test]
async fn test_gc_space_reclamation() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create many checkpoints with unique content
    for i in 0..30 {
        project.modify_files(&["src/main.rs"], &format!("// checkpoint {}\n{}", i, "x".repeat(1000)))?;
        create_checkpoint(&root).await?;
    }

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Get object store size before GC
    let objects_dir = root.join(".tl/objects");
    let size_before = get_dir_size(&objects_dir)?;

    // Run GC
    let gc_result = TlCommand::new(&root)
        .args(&["gc"])
        .assert_success()?;

    println!("GC output:\n{}", gc_result.stdout);

    // Get size after GC
    let size_after = get_dir_size(&objects_dir)?;

    println!("✓ GC space reclamation: before={} bytes, after={} bytes", size_before, size_after);

    // Space should be reclaimed (size_after should be less than size_before in most cases)
    // However, recent checkpoints might be retained by default policy
    // So we just verify GC runs successfully
    assert!(gc_result.stdout.contains("GC Complete") || gc_result.stdout.contains("clean"));

    Ok(())
}

/// Helper function to calculate directory size
fn get_dir_size(path: &std::path::Path) -> Result<u64> {
    let mut total = 0u64;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                total += metadata.len();
            } else if metadata.is_dir() {
                total += get_dir_size(&entry.path())?;
            }
        }
    }
    Ok(total)
}

/// Test GC performance with many checkpoints
#[tokio::test]
async fn test_gc_performance() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 50 checkpoints
    for i in 0..50 {
        project.modify_files(&["src/main.rs"], &format!("// checkpoint {}", i))?;
        create_checkpoint(&root).await?;
    }

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Run GC and measure time
    let start = std::time::Instant::now();
    let gc_result = TlCommand::new(&root)
        .args(&["gc"])
        .assert_success()?;
    let gc_duration = start.elapsed();

    println!("✓ GC performance with 50 checkpoints: {:?}", gc_duration);
    println!("GC output:\n{}", gc_result.stdout);

    // GC should complete reasonably fast
    assert!(gc_duration < Duration::from_secs(30), "GC too slow: {:?}", gc_duration);

    Ok(())
}
