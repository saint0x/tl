//! Edge case tests for deep checkpoint history
//!
//! Tests performance with 100, 500, and 1000+ checkpoints.

use anyhow::Result;
use std::fs;
use std::time::Duration;
use crate::common::{ProjectSize, ProjectTemplate, TestProject};
use crate::common::cli::TlCommand;

/// Trigger checkpoint creation and return the checkpoint ID
///
/// Retries with backoff to handle FSEvents latency and coalescing.
async fn create_checkpoint(root: &std::path::Path) -> Result<Option<String>> {
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
    }

    Ok(None)
}

/// Create checkpoint with guaranteed success (retries with file re-touch)
async fn create_checkpoint_guaranteed(
    root: &std::path::Path,
    project: &mut TestProject,
    file: &str,
    content: &str,
) -> Result<String> {
    // Try up to 12 times with file re-touching (FSEvents can lag under load).
    for attempt in 0..12 {
        // Re-touch the file to ensure FSEvents sees it
        project.modify_files(&[file], content)?;

        let delay = Duration::from_millis(400 + (attempt as u64 * 200));
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

    anyhow::bail!("Failed to create checkpoint after multiple attempts for content: {}", content)
}

/// Test 100 checkpoints - log query performance
#[tokio::test]
async fn test_deep_history_100_checkpoints() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("Creating 100 checkpoints...");
    let mut checkpoint_ids = Vec::new();
    let creation_start = std::time::Instant::now();

    for i in 0..100 {
        let content = format!("// checkpoint {}", i);
        let cp = create_checkpoint_guaranteed(&root, &mut project, "src/main.rs", &content).await?;
        checkpoint_ids.push(cp);

        if (i + 1) % 20 == 0 {
            println!("  Created {} / 100 checkpoints", i + 1);
        }
    }

    let creation_duration = creation_start.elapsed();
    assert_eq!(checkpoint_ids.len(), 100, "Should create 100 checkpoints");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    println!("✓ Created 100 checkpoints in {:?} (avg: {:?}/checkpoint)",
        creation_duration, creation_duration / 100);

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

    let start = std::time::Instant::now();
    let log_100 = TlCommand::new(&root)
        .args(&["log", "--limit", "100"])
        .assert_success()?;
    let log_100_duration = start.elapsed();

    println!("✓ Log query performance:");
    println!("  10-limit:  {:?}", log_10_duration);
    println!("  50-limit:  {:?}", log_50_duration);
    println!("  100-limit: {:?}", log_100_duration);

    // Performance assertions
    assert!(log_10_duration < Duration::from_secs(1), "10-limit log too slow: {:?}", log_10_duration);
    assert!(log_50_duration < Duration::from_secs(1), "50-limit log too slow: {:?}", log_50_duration);
    assert!(log_100_duration < Duration::from_secs(2), "100-limit log too slow: {:?}", log_100_duration);

    // Verify log contains expected patterns
    assert!(log_10.stdout.contains("Checkpoint History"));
    assert!(log_100.stdout.contains("Checkpoint History"));

    // Test restore to various points in history
    let first = &checkpoint_ids[0];
    let middle = &checkpoint_ids[49];
    let last = &checkpoint_ids[99];

    // Restore to middle
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", middle])
        .stdin("y\n")
        .assert_success()?;
    let restore_middle_duration = restore_start.elapsed();

    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("checkpoint 49"), "Should restore to checkpoint 49");

    // Restore to first
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", first])
        .stdin("y\n")
        .assert_success()?;
    let restore_first_duration = restore_start.elapsed();

    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("checkpoint 0"), "Should restore to checkpoint 0");

    // Restore to last
    let restore_start = std::time::Instant::now();
    TlCommand::new(&root)
        .args(&["restore", last])
        .stdin("y\n")
        .assert_success()?;
    let restore_last_duration = restore_start.elapsed();

    let content = fs::read_to_string(root.join("src/main.rs"))?;
    assert!(content.contains("checkpoint 99"), "Should restore to checkpoint 99");

    println!("✓ Restore performance across 100 checkpoints:");
    println!("  to first:  {:?}", restore_first_duration);
    println!("  to middle: {:?}", restore_middle_duration);
    println!("  to last:   {:?}", restore_last_duration);

    // All restores should be fast regardless of position
    assert!(restore_first_duration < Duration::from_secs(5), "Restore to first too slow");
    assert!(restore_middle_duration < Duration::from_secs(5), "Restore to middle too slow");
    assert!(restore_last_duration < Duration::from_secs(5), "Restore to last too slow");

    Ok(())
}

/// Test 500 checkpoints - scaling behavior
#[tokio::test]
#[ignore] // Ignore by default due to time requirements - run with --ignored
async fn test_deep_history_500_checkpoints() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("Creating 500 checkpoints (this may take several minutes)...");
    let mut checkpoint_ids = Vec::new();
    let creation_start = std::time::Instant::now();

    for i in 0..500 {
        project.modify_files(&["src/main.rs"], &format!("// checkpoint {}", i))?;
        if let Some(cp) = create_checkpoint(&root).await? {
            checkpoint_ids.push(cp);
        }
        if (i + 1) % 50 == 0 {
            println!("  Created {} / 500 checkpoints ({:.1}%)",
                i + 1, ((i + 1) as f64 / 500.0) * 100.0);
        }
    }

    let creation_duration = creation_start.elapsed();
    assert_eq!(checkpoint_ids.len(), 500, "Should create 500 checkpoints");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    println!("✓ Created 500 checkpoints in {:?} (avg: {:?}/checkpoint)",
        creation_duration, creation_duration / 500);

    // Test log query performance
    let start = std::time::Instant::now();
    let log_result = TlCommand::new(&root)
        .args(&["log", "--limit", "100"])
        .assert_success()?;
    let log_duration = start.elapsed();

    println!("✓ Log query (100-limit) with 500 total: {:?}", log_duration);
    assert!(log_duration < Duration::from_secs(2), "Log query too slow: {:?}", log_duration);
    assert!(log_result.stdout.contains("Checkpoint History"));

    // Test restore to various points
    let positions = [0, 100, 250, 400, 499];

    for &pos in &positions {
        let checkpoint = &checkpoint_ids[pos];
        let restore_start = std::time::Instant::now();
        TlCommand::new(&root)
            .args(&["restore", checkpoint])
            .stdin("y\n")
            .assert_success()?;
        let restore_duration = restore_start.elapsed();

        let content = fs::read_to_string(root.join("src/main.rs"))?;
        assert!(content.contains(&format!("checkpoint {}", pos)),
            "Should restore to checkpoint {}", pos);

        println!("  Restore to position {}: {:?}", pos, restore_duration);
        assert!(restore_duration < Duration::from_secs(5),
            "Restore to position {} too slow: {:?}", pos, restore_duration);
    }

    println!("✓ All restores in deep 500-checkpoint history performed well");

    Ok(())
}

/// Test 1000+ checkpoints - extreme scaling
#[tokio::test]
#[ignore] // Ignore by default due to time requirements - run with --ignored
async fn test_deep_history_1000_checkpoints() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("Creating 1000 checkpoints (this will take a while)...");
    let mut checkpoint_ids = Vec::new();
    let creation_start = std::time::Instant::now();

    for i in 0..1000 {
        project.modify_files(&["src/main.rs"], &format!("// checkpoint {}", i))?;
        if let Some(cp) = create_checkpoint(&root).await? {
            checkpoint_ids.push(cp);
        }
        if (i + 1) % 100 == 0 {
            println!("  Created {} / 1000 checkpoints ({:.1}%)",
                i + 1, ((i + 1) as f64 / 1000.0) * 100.0);
        }
    }

    let creation_duration = creation_start.elapsed();
    assert_eq!(checkpoint_ids.len(), 1000, "Should create 1000 checkpoints");

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    println!("✓ Created 1000 checkpoints in {:?} (avg: {:?}/checkpoint)",
        creation_duration, creation_duration / 1000);

    // Test log query performance - should still be fast!
    let start = std::time::Instant::now();
    let log_result = TlCommand::new(&root)
        .args(&["log", "--limit", "100"])
        .assert_success()?;
    let log_duration = start.elapsed();

    println!("✓ Log query (100-limit) with 1000 total: {:?}", log_duration);
    assert!(log_duration < Duration::from_secs(3), "Log query too slow even with 1000 checkpoints: {:?}", log_duration);
    assert!(log_result.stdout.contains("Checkpoint History"));

    // Test restore to various points across entire history
    let positions = [0, 250, 500, 750, 999];

    for &pos in &positions {
        let checkpoint = &checkpoint_ids[pos];
        let restore_start = std::time::Instant::now();
        TlCommand::new(&root)
            .args(&["restore", checkpoint])
            .stdin("y\n")
            .assert_success()?;
        let restore_duration = restore_start.elapsed();

        let content = fs::read_to_string(root.join("src/main.rs"))?;
        assert!(content.contains(&format!("checkpoint {}", pos)),
            "Should restore to checkpoint {}", pos);

        println!("  Restore to position {}: {:?}", pos, restore_duration);
        assert!(restore_duration < Duration::from_secs(5),
            "Restore to position {} too slow: {:?}", pos, restore_duration);
    }

    // Test GC performance with deep history
    println!("Running GC on 1000-checkpoint history...");
    let gc_start = std::time::Instant::now();
    let gc_result = TlCommand::new(&root)
        .args(&["gc"])
        .assert_success()?;
    let gc_duration = gc_start.elapsed();

    println!("✓ GC with 1000 checkpoints: {:?}", gc_duration);
    println!("  {}", gc_result.stdout.lines().next().unwrap_or(""));

    assert!(gc_duration < Duration::from_secs(120),
        "GC with 1000 checkpoints too slow: {:?}", gc_duration);

    println!("✓ All operations in extreme 1000-checkpoint history performed well");

    Ok(())
}

/// Test restore performance remains constant regardless of history depth
#[tokio::test]
async fn test_restore_scaling_independence() -> Result<()> {
    let mut project = TestProject::new(
        ProjectTemplate::rust_project(ProjectSize::Tiny)
    )?;
    let root = project.root().to_path_buf();

    TlCommand::new(&root).args(&["init"]).assert_success()?;
    TlCommand::new(&root).args(&["start"]).assert_success()?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Measure restore time at different history depths
    let mut restore_times = Vec::new();

    // Checkpoints at different depths: 10, 30, 60, 100
    let test_depths = [10, 30, 60, 100];

    for &depth in &test_depths {
        // Create checkpoints up to this depth
        let current_count = restore_times.len() * 30;
        let checkpoints_to_create = if depth > current_count {
            depth - current_count
        } else {
            0
        };

        let mut target_checkpoint = None;

        for i in 0..checkpoints_to_create {
            project.modify_files(&["src/main.rs"], &format!("// depth {} checkpoint {}", depth, i))?;
            if let Some(cp) = create_checkpoint(&root).await? {
                // Save the first checkpoint of this batch as our restore target
                if i == 0 {
                    target_checkpoint = Some(cp);
                }
            }
        }

        if let Some(checkpoint) = target_checkpoint {
            // Modify file
            project.modify_files(&["src/main.rs"], "// MODIFIED")?;

            // Measure restore time
            TlCommand::new(&root).args(&["stop"]).assert_success()?;

            let restore_start = std::time::Instant::now();
            TlCommand::new(&root)
                .args(&["restore", &checkpoint])
                .stdin("y\n")
                .assert_success()?;
            let restore_duration = restore_start.elapsed();

            restore_times.push((depth, restore_duration));
            println!("Restore at depth {}: {:?}", depth, restore_duration);

            TlCommand::new(&root).args(&["start"]).assert_success()?;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    TlCommand::new(&root).args(&["stop"]).assert_success()?;

    // Verify restore times don't degrade significantly
    let times: Vec<Duration> = restore_times.iter().map(|(_, d)| *d).collect();
    let avg_time = times.iter().sum::<Duration>() / times.len() as u32;

    println!();
    println!("✓ Restore scaling independence:");
    for (depth, time) in &restore_times {
        let ratio = time.as_secs_f64() / avg_time.as_secs_f64();
        println!("  Depth {:3}: {:?} ({}x avg)", depth, time, format!("{:.2}", ratio));
    }

    // All restore times should be within 3x of average (accounting for variance)
    for (depth, time) in &restore_times {
        let ratio = time.as_secs_f64() / avg_time.as_secs_f64();
        assert!(ratio < 3.0,
            "Restore at depth {} took {}x average time (too slow)", depth, ratio);
    }

    println!("✓ Restore performance independent of history depth");

    Ok(())
}
