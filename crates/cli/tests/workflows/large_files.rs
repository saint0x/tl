//! Edge case tests for large file handling
//!
//! Tests checkpoint and restore performance with files from 10MB to 1GB.

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

/// Test 10MB file handling
#[tokio::test]
async fn test_large_file_10mb() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 10MB file with repeating pattern
    let large_file = root.join("large_10mb.bin");
    let content: Vec<u8> = (0..10 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
    fs::write(&large_file, &content)?;

    println!("Created 10MB file");

    let start = std::time::Instant::now();
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for 10MB file");
    let checkpoint_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify file
    fs::write(&large_file, vec![0xAA; 10 * 1024 * 1024])?;

    // Restore
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;
    let restore_duration = restore_start.elapsed();

    // Verify - check size and sample bytes
    let restored = fs::read(&large_file)?;
    assert_eq!(restored.len(), 10 * 1024 * 1024, "File size should match");
    assert_eq!(restored[0], 0, "First byte should match");
    assert_eq!(restored[1000], (1000 % 256) as u8, "Sample byte should match");
    assert_eq!(restored[1_000_000], ((1_000_000) % 256) as u8, "Mid-file byte should match");

    println!("✓ 10MB file: checkpoint={:?}, restore={:?}", checkpoint_duration, restore_duration);

    // Performance assertions
    assert!(checkpoint_duration < Duration::from_secs(30), "10MB checkpoint too slow: {:?}", checkpoint_duration);
    assert!(restore_duration < Duration::from_secs(30), "10MB restore too slow: {:?}", restore_duration);

    Ok(())
}

/// Test 100MB file handling
#[tokio::test]
async fn test_large_file_100mb() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 100MB file
    let large_file = root.join("large_100mb.bin");

    println!("Creating 100MB file...");
    // Write in chunks to avoid huge memory allocation
    {
        use std::io::Write;
        let mut file = fs::File::create(&large_file)?;
        let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect(); // 1MB chunk
        for _ in 0..100 {
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
    }

    println!("Created 100MB file");

    let start = std::time::Instant::now();
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for 100MB file");
    let checkpoint_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify file (write zeros)
    {
        use std::io::Write;
        let mut file = fs::File::create(&large_file)?;
        let zeros = vec![0u8; 1024 * 1024]; // 1MB of zeros
        for _ in 0..100 {
            file.write_all(&zeros)?;
        }
        file.sync_all()?;
    }

    // Restore
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;
    let restore_duration = restore_start.elapsed();

    // Verify - check size and sample bytes
    let metadata = fs::metadata(&large_file)?;
    assert_eq!(metadata.len(), 100 * 1024 * 1024, "File size should match");

    // Sample specific bytes without reading entire file
    use std::io::{Seek, Read};
    let mut file = fs::File::open(&large_file)?;

    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], 0, "First byte should match");

    file.seek(std::io::SeekFrom::Start(1000))?;
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], (1000 % 256) as u8, "Sample byte should match");

    file.seek(std::io::SeekFrom::Start(50_000_000))?;
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], ((50_000_000) % 256) as u8, "Mid-file byte should match");

    println!("✓ 100MB file: checkpoint={:?}, restore={:?}", checkpoint_duration, restore_duration);

    // Performance assertions - more lenient for 100MB
    assert!(checkpoint_duration < Duration::from_secs(60), "100MB checkpoint too slow: {:?}", checkpoint_duration);
    assert!(restore_duration < Duration::from_secs(60), "100MB restore too slow: {:?}", restore_duration);

    Ok(())
}

/// Test 1GB file handling (stress test)
#[tokio::test]
#[ignore] // Ignore by default due to time/space requirements - run with --ignored
async fn test_large_file_1gb() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create 1GB file
    let large_file = root.join("large_1gb.bin");

    println!("Creating 1GB file (this may take a while)...");
    // Write in chunks to avoid huge memory allocation
    {
        use std::io::Write;
        let mut file = fs::File::create(&large_file)?;
        let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect(); // 1MB chunk
        for i in 0..1024 {
            file.write_all(&chunk)?;
            if i % 100 == 0 {
                println!("  Written {} MB / 1024 MB", i);
            }
        }
        file.sync_all()?;
    }

    println!("Created 1GB file");

    let start = std::time::Instant::now();
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for 1GB file");
    let checkpoint_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Modify file (write zeros)
    println!("Modifying 1GB file...");
    {
        use std::io::Write;
        let mut file = fs::File::create(&large_file)?;
        let zeros = vec![0u8; 1024 * 1024]; // 1MB of zeros
        for _ in 0..1024 {
            file.write_all(&zeros)?;
        }
        file.sync_all()?;
    }

    // Restore
    println!("Restoring 1GB file...");
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;
    let restore_duration = restore_start.elapsed();

    // Verify - check size and sample bytes
    let metadata = fs::metadata(&large_file)?;
    assert_eq!(metadata.len(), 1024 * 1024 * 1024, "File size should match");

    // Sample specific bytes without reading entire file
    use std::io::{Seek, Read};
    let mut file = fs::File::open(&large_file)?;

    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], 0, "First byte should match");

    file.seek(std::io::SeekFrom::Start(1000))?;
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], (1000 % 256) as u8, "Sample byte should match");

    file.seek(std::io::SeekFrom::Start(500_000_000))?;
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], ((500_000_000) % 256) as u8, "Mid-file byte should match");

    file.seek(std::io::SeekFrom::Start(1024 * 1024 * 1024 - 1))?;
    file.read_exact(&mut byte)?;
    assert_eq!(byte[0], ((1024 * 1024 * 1024 - 1) % 256) as u8, "Last byte should match");

    println!("✓ 1GB file: checkpoint={:?}, restore={:?}", checkpoint_duration, restore_duration);

    // Performance assertions - very lenient for 1GB
    assert!(checkpoint_duration < Duration::from_secs(300), "1GB checkpoint too slow: {:?}", checkpoint_duration);
    assert!(restore_duration < Duration::from_secs(300), "1GB restore too slow: {:?}", restore_duration);

    Ok(())
}

/// Test multiple large files in one checkpoint
#[tokio::test]
async fn test_multiple_large_files() -> Result<()> {
    let project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create three 5MB files
    for i in 1..=3 {
        let file_path = root.join(format!("large_{}.bin", i));
        let content: Vec<u8> = (0..5 * 1024 * 1024).map(|j| ((i * 1000 + j) % 256) as u8).collect();
        fs::write(&file_path, &content)?;
    }

    println!("Created 3 x 5MB files");

    let start = std::time::Instant::now();
    let checkpoint = create_checkpoint(&root).await?
        .expect("Should create checkpoint for multiple large files");
    let checkpoint_duration = start.elapsed();

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Delete all files
    for i in 1..=3 {
        fs::remove_file(root.join(format!("large_{}.bin", i)))?;
    }

    // Restore
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", &checkpoint])
        .stdin("y\n")
        .assert_success()?;
    let restore_duration = restore_start.elapsed();

    // Verify all files restored correctly
    for i in 1..=3 {
        let file_path = root.join(format!("large_{}.bin", i));
        assert!(file_path.exists(), "File {} should be restored", i);

        let metadata = fs::metadata(&file_path)?;
        assert_eq!(metadata.len(), 5 * 1024 * 1024, "File {} size should match", i);

        // Check first byte
        let content = fs::read(&file_path)?;
        assert_eq!(content[0], ((i * 1000) % 256) as u8, "File {} content should match", i);
    }

    println!("✓ Multiple large files (3 x 5MB): checkpoint={:?}, restore={:?}", checkpoint_duration, restore_duration);

    Ok(())
}
