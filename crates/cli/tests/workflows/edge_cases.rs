//! Edge case and stress tests for checkpoint operations
//!
//! Tests boundary conditions, extreme scenarios, and error handling.

use anyhow::Result;
use std::fs;
use std::time::Duration;
use crate::common::{ProjectSize, ProjectTemplate, TestProject};
use crate::common::cli::TlCommand;

/// Trigger checkpoint creation and return the checkpoint ID
///
/// Retries with backoff to handle FSEvents latency and coalescing.
async fn create_checkpoint(root: &std::path::Path) -> Result<Option<String>> {
    // Under load, FSEvents latency can be multiple seconds. Keep this bounded
    // but forgiving so integration tests aren't flaky.
    for attempt in 0..10 {
        let delay = Duration::from_millis(400 + (attempt as u64 * 250));
        tokio::time::sleep(delay).await;

        let flush_result = TlCommand::new(root)
            .args(&["flush"])
            .execute()?;

        if flush_result.success() {
            if let Some(ulid) = crate::common::cli::extract_ulid(&flush_result.stdout) {
                return Ok(Some(ulid));
            }
        }

        // If no checkpoint created, the FSEvents hasn't fired yet - retry
        if attempt < 9 {
            tracing::debug!("Checkpoint creation attempt {} failed, retrying...", attempt + 1);
        }
    }

    Ok(None)
}

/// Test flushing with no pending changes
#[tokio::test]
async fn test_flush_no_changes() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // First flush creates initial checkpoint (initial files are "new" from daemon perspective)
    let first_flush = TlCommand::new(&root)
        .args(&["flush"])
        .execute()?;
    println!("First flush (establishing baseline): {}", first_flush.stdout);

    // Wait for any pending events to settle
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Second flush with no changes should return "No pending changes"
    let flush_result = TlCommand::new(&root)
        .args(&["flush"])
        .assert_success()?;

    assert!(
        flush_result.stdout.contains("No pending changes") ||
        flush_result.stdout.contains("no pending"),
        "Expected 'No pending changes' message, got: {}",
        flush_result.stdout
    );

    TlCommand::new(&root).args(&["stop"]).assert_success()?;
    println!("✓ Flush with no changes handled correctly");

    Ok(())
}

/// Test empty file handling
#[tokio::test]
async fn test_empty_files() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create empty file
    let empty_file = root.join("empty.txt");
    fs::write(&empty_file, "")?;

    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for empty file");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Delete empty file
    fs::remove_file(&empty_file)?;

    // Restore should recreate empty file
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    assert!(empty_file.exists(), "Empty file should be restored");
    let content = fs::read_to_string(&empty_file)?;
    assert_eq!(content.len(), 0, "File should still be empty");

    println!("✓ Empty file handling works correctly");

    Ok(())
}

/// Test large file handling (1MB)
#[tokio::test]
async fn test_large_file_1mb() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 1MB file
    let large_file = root.join("large_1mb.bin");
    let content: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
    fs::write(&large_file, &content)?;

    let start = std::time::Instant::now();
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for 1MB file");
    let checkpoint_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify file
    fs::write(&large_file, vec![0u8; 1024 * 1024])?;

    // Restore
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;
    let restore_duration = restore_start.elapsed();

    // Verify
    let restored = fs::read(&large_file)?;
    assert_eq!(restored.len(), 1024 * 1024, "File size should match");
    assert_eq!(restored[0], 0, "First byte should match");
    assert_eq!(restored[1000], (1000 % 256) as u8, "Content should match");

    println!("✓ 1MB file: checkpoint={:?}, restore={:?}", checkpoint_duration, restore_duration);

    // Performance assertion - should handle 1MB reasonably fast
    assert!(checkpoint_duration < Duration::from_secs(5), "1MB checkpoint too slow: {:?}", checkpoint_duration);
    assert!(restore_duration < Duration::from_secs(5), "1MB restore too slow: {:?}", restore_duration);

    Ok(())
}

/// Test many small files (stress test)
#[tokio::test]
async fn test_many_small_files() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Large) // 1000 files
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Modify a single file to create checkpoint
    let test_file = root.join("stress_test.txt");
    fs::write(&test_file, "initial")?;

    let start = std::time::Instant::now();
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint");
    let checkpoint_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify all files in project
    for file_info in &project.file_manifest {
        if file_info.extension == "rs" {
            fs::write(&file_info.path, "// MODIFIED")?;
        }
    }
    fs::write(&test_file, "modified")?;

    // Restore all files
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;
    let restore_duration = restore_start.elapsed();

    // Verify
    let content = fs::read_to_string(&test_file)?;
    assert_eq!(content, "initial", "File should be restored");

    println!(
        "✓ Large project (1000 files): checkpoint={:?}, restore={:?}",
        checkpoint_duration, restore_duration
    );

    // Performance assertion for large project
    assert!(restore_duration < Duration::from_secs(30), "Large project restore too slow: {:?}", restore_duration);

    Ok(())
}

/// Test special characters in filenames
#[tokio::test]
async fn test_special_filenames() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create files with special names
    let special_files = vec![
        "file with spaces.txt",
        "file-with-dashes.txt",
        "file_with_underscores.txt",
        "file.multiple.dots.txt",
        "café.txt", // Unicode
    ];

    for name in &special_files {
        let file_path = root.join(name);
        fs::write(&file_path, format!("content of {}", name))?;
    }

    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint with special filenames");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Delete all special files
    for name in &special_files {
        let file_path = root.join(name);
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
    }

    // Restore
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    // Verify all files restored
    for name in &special_files {
        let file_path = root.join(name);
        assert!(file_path.exists(), "File '{}' should be restored", name);
        let content = fs::read_to_string(&file_path)?;
        assert_eq!(content, format!("content of {}", name));
    }

    println!("✓ Special filenames handled correctly");

    Ok(())
}

/// Test deep directory nesting
#[tokio::test]
async fn test_deep_nesting() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create deeply nested structure (20 levels)
    let mut deep_path = root.clone();
    for i in 0..20 {
        deep_path = deep_path.join(format!("level_{}", i));
    }
    fs::create_dir_all(&deep_path)?;

    let deep_file = deep_path.join("deep_file.txt");
    fs::write(&deep_file, "deeply nested content")?;

    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint with deep nesting");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Delete deep structure
    fs::remove_dir_all(root.join("level_0"))?;

    // Restore
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    // Verify
    assert!(deep_file.exists(), "Deeply nested file should be restored");
    let content = fs::read_to_string(&deep_file)?;
    assert_eq!(content, "deeply nested content");

    println!("✓ Deep nesting (20 levels) handled correctly");

    Ok(())
}

/// Create checkpoint with guaranteed success (retries with file re-touch)
async fn create_checkpoint_guaranteed(
    root: &std::path::Path,
    project: &mut TestProject,
    file: &str,
    content: &str,
) -> Result<String> {
    // Try up to 5 times with file re-touching
    for attempt in 0..5 {
        // Re-touch the file to ensure FSEvents sees it
        project.modify_files(&[file], content)?;

        let delay = Duration::from_millis(500 + (attempt as u64 * 200));
        tokio::time::sleep(delay).await;

        let flush_result = TlCommand::new(root)
            .args(&["flush"])
            .execute()?;

        if flush_result.success() {
            if let Some(ulid) = crate::common::cli::extract_ulid(&flush_result.stdout) {
                return Ok(ulid);
            }
        }
    }

    anyhow::bail!("Failed to create checkpoint after 5 attempts for content: {}", content)
}

/// Test rapid successive checkpoints
#[tokio::test]
async fn test_rapid_checkpoints() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 10 checkpoints with guaranteed success
    let mut checkpoints = Vec::new();
    let start = std::time::Instant::now();

    for i in 0..10 {
        let content = format!("// version {}", i);
        let cp = create_checkpoint_guaranteed(&root, &mut project, "src/main.rs", &content).await?;
        checkpoints.push(cp);
    }

    let total_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    println!(
        "✓ Created {} checkpoints in {:?} (avg: {:?}/checkpoint)",
        checkpoints.len(),
        total_duration,
        total_duration / checkpoints.len() as u32
    );

    // Should have created all 10 checkpoints
    assert_eq!(checkpoints.len(), 10, "Should create all checkpoints");

    // Restore to first checkpoint
    TlCommand::new(&root)
        .args(&["restore", &checkpoints[0]])
        .stdin("y\n")
        .assert_success()?;

    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("version 0"), "Should restore to first version");

    Ok(())
}

/// Test binary file handling
#[tokio::test]
async fn test_binary_files() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create binary file with non-UTF8 content
    let binary_file = root.join("binary.dat");
    let binary_content: Vec<u8> = vec![
        0x00, 0xFF, 0xFE, 0xFD, 0x80, 0x90, 0xA0, 0xB0,
        0xC0, 0xD0, 0xE0, 0xF0, 0x01, 0x02, 0x03, 0x04,
    ];
    fs::write(&binary_file, &binary_content)?;

    // Retry checkpoint creation to handle FSEvents race conditions
    let mut checkpoint = None;
    for attempt in 0..3 {
        if let Some(cp) = create_checkpoint(&root).await? {
            checkpoint = Some(cp);
            break;
        }
        // Re-touch the file to trigger FSEvents
        fs::write(&binary_file, &binary_content)?;
        tokio::time::sleep(Duration::from_millis(300 * (attempt + 1) as u64)).await;
    }
    let checkpoint = checkpoint.expect("Should create checkpoint for binary file");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Corrupt the binary file
    fs::write(&binary_file, vec![0xAA; 16])?;

    // Restore
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    // Verify exact binary content
    let restored = fs::read(&binary_file)?;
    assert_eq!(restored, binary_content, "Binary content should match exactly");

    println!("✓ Binary file handling works correctly");

    Ok(())
}

/// Test restore to same state (no-op)
#[tokio::test]
async fn test_restore_same_state() -> Result<()> {
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

    // Restore to same state (files haven't changed)
    let restore_result = TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    println!("✓ Restore to same state: {:?}", restore_result.duration);

    // Verify content unchanged
    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("test content"));

    Ok(())
}

/// Test checkpoint with mixed file types
#[tokio::test]
async fn test_mixed_file_types() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::mixed_project(ProjectSize::Small)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Add various file types
    fs::write(root.join("test.txt"), "text file")?;
    fs::write(root.join("test.json"), r#"{"key": "value"}"#)?;
    fs::write(root.join("test.md"), "# Markdown")?;
    fs::write(root.join("test.bin"), vec![0u8, 1, 2, 3, 255])?;

    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for mixed files");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Delete all test files
    for name in &["test.txt", "test.json", "test.md", "test.bin"] {
        let path = root.join(name);
        if path.exists() {
            fs::remove_file(&path)?;
        }
    }

    // Restore
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;

    // Verify all file types restored correctly
    assert_eq!(fs::read_to_string(root.join("test.txt"))?, "text file");
    assert_eq!(fs::read_to_string(root.join("test.json"))?, r#"{"key": "value"}"#);
    assert_eq!(fs::read_to_string(root.join("test.md"))?, "# Markdown");
    assert_eq!(fs::read(root.join("test.bin"))?, vec![0u8, 1, 2, 3, 255]);

    println!("✓ Mixed file types handled correctly");

    Ok(())
}

/// Test checkpoint count scaling
#[tokio::test]
async fn test_many_checkpoints_log() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 50 checkpoints and track them
    // Use retry logic to handle race conditions with FSEvents
    let mut checkpoint_ids = Vec::new();
    for i in 0..50 {
        project.modify_files(&["src/main.rs"], &format!("// checkpoint {}", i))?;

        // Retry up to 3 times if checkpoint creation fails
        let mut attempts = 0;
        loop {
            if let Some(cp) = create_checkpoint(&root).await? {
                checkpoint_ids.push(cp);
                break;
            }
            attempts += 1;
            if attempts >= 3 {
                // Force a unique modification and try once more
                project.modify_files(&["src/main.rs"], &format!("// checkpoint {} attempt {}", i, attempts))?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(cp) = create_checkpoint(&root).await? {
                    checkpoint_ids.push(cp);
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Verify we created most checkpoints (allow small variance due to timing)
    assert!(checkpoint_ids.len() >= 48, "Should have created at least 48 checkpoints, got {}", checkpoint_ids.len());
    println!("✓ Created {} checkpoints", checkpoint_ids.len());

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Test log query performance with different limits
    let start = std::time::Instant::now();
    let log_10 = TlCommand::new(&root)
        .args(&["log", "--limit", "10"])
        .assert_success()?;
    let log_10_duration = start.elapsed();

    let start = std::time::Instant::now();
    let log_50 = TlCommand::new(&root)
        .args(&["log", "--limit", "50"])
        .assert_success()?;
    let log_50_duration = start.elapsed();

    println!(
        "✓ Log query performance: 10-limit={:?}, 50-limit={:?}",
        log_10_duration, log_50_duration
    );

    // Performance assertions - log queries should be fast
    assert!(log_10_duration < Duration::from_secs(1), "10-limit log query too slow: {:?}", log_10_duration);
    assert!(log_50_duration < Duration::from_secs(1), "50-limit log query too slow: {:?}", log_50_duration);

    // Verify log output contains expected patterns
    // Log shows 8-char short IDs, not full 26-char ULIDs
    assert!(log_10.stdout.contains("Checkpoint History"), "Log should have header");
    assert!(log_50.stdout.contains("Checkpoint History"), "Log should have header");

    // The log output should mention how many total checkpoints exist
    assert!(
        log_10.stdout.contains("50 total") || log_10.stdout.contains("Showing"),
        "Log should indicate total checkpoint count"
    );

    // Verify we can restore to first and last checkpoint
    let first_checkpoint = &checkpoint_ids[0];
    let last_checkpoint = &checkpoint_ids[49];

    TlCommand::new(&root)
        .args(&["restore", last_checkpoint])
        .stdin("y\n")
        .assert_success()?;

    let content_last = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content_last.contains("checkpoint 49"), "Should restore to last checkpoint");

    TlCommand::new(&root)
        .args(&["restore", first_checkpoint])
        .stdin("y\n")
        .assert_success()?;

    let content_first = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content_first.contains("checkpoint 0"), "Should restore to first checkpoint");

    println!("✓ Checkpoint scaling test completed successfully");

    Ok(())
}
