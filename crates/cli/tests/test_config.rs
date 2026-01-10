//! Tests for config command

use anyhow::Result;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn test_config_list() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--list")
        .output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show all config sections
    assert!(stdout.contains("[daemon]"));
    assert!(stdout.contains("[gc]"));
    assert!(stdout.contains("checkpoint_interval_secs"));
    assert!(stdout.contains("auto_gc_enabled"));
    assert!(stdout.contains("retain_count"));

    Ok(())
}

#[test]
fn test_config_get() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--get")
        .arg("daemon.checkpoint_interval_secs")
        .output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should return a numeric value
    let value: u64 = stdout.trim().parse()?;
    assert!(value >= 1 && value <= 3600);

    Ok(())
}

#[test]
fn test_config_get_unknown_key() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--get")
        .arg("daemon.unknown_key")
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Unknown config key"));

    Ok(())
}

#[test]
fn test_config_set_invalid_value() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--set")
        .arg("daemon.checkpoint_interval_secs=999999")  // Too high
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Invalid") || stderr.contains("validation"));

    Ok(())
}

#[test]
fn test_config_path() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--path")
        .output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show config file path
    assert!(stdout.contains("config.toml") || stdout.contains(".config/tl"));

    Ok(())
}

#[test]
fn test_config_example() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_tl"))
        .arg("config")
        .arg("--example")
        .output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show example TOML content
    assert!(stdout.contains("[daemon]"));
    assert!(stdout.contains("[gc]"));
    assert!(stdout.contains("checkpoint_interval_secs"));

    Ok(())
}
