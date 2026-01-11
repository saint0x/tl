//! Stash management for saving and restoring work-in-progress changes
//!
//! Implements Git-style stash operations using Timelapse checkpoints and pins.
//! Stashes are stored as pinned checkpoints with a special "stash/" prefix.

use crate::util;
use crate::data_access;
use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use tl_core::store::Store;
use journal::{Checkpoint, PinManager};
use ulid::Ulid;

/// List all stashes
pub async fn run_list() -> Result<()> {
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    // Ensure daemon is running for data access
    crate::daemon::ensure_daemon_running().await?;

    // Get all pins (PinManager doesn't require journal lock)
    let pins = PinManager::new(&tl_dir);
    let all_pins = pins.list_pins()?;

    // Filter stash pins (those starting with "stash/")
    let mut stashes: Vec<(String, Ulid)> = all_pins.into_iter()
        .filter(|(name, _)| name.starts_with("stash/"))
        .collect();

    if stashes.is_empty() {
        println!("{}", "No stashes found".dimmed());
        return Ok(());
    }

    // Get all checkpoint data via data_access layer
    let checkpoint_ids: Vec<Ulid> = stashes.iter().map(|(_, id)| *id).collect();
    let checkpoints = data_access::get_checkpoints(&checkpoint_ids, &tl_dir).await?;

    // Build map of id -> checkpoint
    let checkpoint_map: std::collections::HashMap<Ulid, Option<Checkpoint>> =
        stashes.iter()
            .map(|(_, id)| *id)
            .zip(checkpoints.into_iter())
            .collect();

    // Sort by checkpoint timestamp (newest first)
    stashes.sort_by(|(_, id_a), (_, id_b)| {
        let cp_a = checkpoint_map.get(id_a).and_then(|o| o.as_ref());
        let cp_b = checkpoint_map.get(id_b).and_then(|o| o.as_ref());

        match (cp_a, cp_b) {
            (Some(a), Some(b)) => b.ts_unix_ms.cmp(&a.ts_unix_ms),
            _ => std::cmp::Ordering::Equal,
        }
    });

    println!("{}", "Stashes:".bold());
    for (index, (name, checkpoint_id)) in stashes.iter().enumerate() {
        let checkpoint = checkpoint_map.get(checkpoint_id)
            .and_then(|o| o.as_ref())
            .context("Stash checkpoint not found")?;

        let time_str = util::format_relative_time(checkpoint.ts_unix_ms);
        let stash_display_name = name.strip_prefix("stash/").unwrap_or(name);

        println!(
            "  {}: {} {} {}",
            format!("stash@{{{}}}", index).yellow(),
            stash_display_name.cyan(),
            "-".dimmed(),
            time_str.dimmed()
        );

        // Show first few changed files
        if !checkpoint.touched_paths.is_empty() {
            let file_count = checkpoint.touched_paths.len();
            let sample = checkpoint.touched_paths.iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");

            if file_count <= 3 {
                println!("      {}", sample.dimmed());
            } else {
                println!("      {} ({} more)", sample.dimmed(), file_count - 3);
            }
        }
    }

    Ok(())
}

/// Save current changes to stash
///
/// Note: Unlike git stash, timelapse always captures all file changes in the
/// working directory. The `include_untracked` parameter exists for CLI parity
/// with git stash but has no effect since timelapse doesn't distinguish between
/// tracked and untracked files - it monitors the entire filesystem.
pub async fn run_push(message: Option<String>, include_untracked: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Note: include_untracked has no effect in timelapse since we track all
    // filesystem changes, not just git-tracked files. Log if user explicitly
    // set the flag so they understand the behavior.
    if include_untracked {
        tracing::debug!(
            "Note: -u/--include-untracked has no effect in timelapse stash. \
             All file changes are automatically captured."
        );
    }

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    let tl_dir = repo_root.join(".tl");
    let pins = PinManager::new(&tl_dir);

    // Create a manual checkpoint to capture current state via IPC
    let socket_path = tl_dir.join("state/daemon.sock");
    let mut client = crate::ipc::IpcClient::connect(&socket_path)
        .await
        .context("Failed to connect to daemon")?;

    let checkpoint_id = match client.flush_checkpoint().await? {
        Some(id_str) => Ulid::from_string(&id_str)?,
        None => {
            println!("{}", "No changes to stash".yellow());
            return Ok(());
        }
    };

    // Pin this checkpoint as a stash
    let stash_name = if let Some(msg) = message {
        format!("stash/{}", msg)
    } else {
        // Auto-generate stash name with timestamp
        format!("stash/WIP-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"))
    };

    pins.pin(&stash_name, checkpoint_id)?;

    println!("{} Saved working directory to {}", "✓".green(), stash_name.cyan());

    // Get checkpoint info via data_access
    let checkpoints = data_access::get_checkpoints(&[checkpoint_id], &tl_dir).await?;
    if let Some(checkpoint) = checkpoints[0].as_ref() {
        if !checkpoint.touched_paths.is_empty() {
            println!(
                "  {} files changed",
                checkpoint.touched_paths.len().to_string().yellow()
            );
        }
    }

    Ok(())
}

/// Apply a stash to the working directory
pub async fn run_apply(stash_ref: Option<String>, pop: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    // Ensure daemon is running for data access
    crate::daemon::ensure_daemon_running().await?;

    let pins = PinManager::new(&tl_dir);

    // Get all stash pins
    let all_pins = pins.list_pins()?;
    let mut stashes: Vec<(String, Ulid)> = all_pins.into_iter()
        .filter(|(name, _)| name.starts_with("stash/"))
        .collect();

    if stashes.is_empty() && stash_ref.is_none() {
        anyhow::bail!("No stashes found");
    }

    // Get checkpoint data for sorting
    let checkpoint_ids: Vec<Ulid> = stashes.iter().map(|(_, id)| *id).collect();
    let checkpoints = data_access::get_checkpoints(&checkpoint_ids, &tl_dir).await?;

    let checkpoint_map: std::collections::HashMap<Ulid, Option<Checkpoint>> =
        stashes.iter()
            .map(|(_, id)| *id)
            .zip(checkpoints.into_iter())
            .collect();

    // Sort by checkpoint timestamp (newest first)
    stashes.sort_by(|(_, id_a), (_, id_b)| {
        let cp_a = checkpoint_map.get(id_a).and_then(|o| o.as_ref());
        let cp_b = checkpoint_map.get(id_b).and_then(|o| o.as_ref());

        match (cp_a, cp_b) {
            (Some(a), Some(b)) => b.ts_unix_ms.cmp(&a.ts_unix_ms),
            _ => std::cmp::Ordering::Equal,
        }
    });

    // Resolve stash reference
    let stash_name = if let Some(ref_str) = stash_ref {
        // Support both "stash@{0}" and "stash/name" formats
        if ref_str.starts_with("stash@{") {
            // Parse index
            let index: usize = ref_str
                .trim_start_matches("stash@{")
                .trim_end_matches('}')
                .parse()
                .context("Invalid stash index")?;

            stashes.get(index)
                .map(|(name, _)| name.clone())
                .ok_or_else(|| anyhow::anyhow!("Stash index {} not found", index))?
        } else if ref_str.starts_with("stash/") {
            ref_str.to_string()
        } else {
            format!("stash/{}", ref_str)
        }
    } else {
        // Default to most recent stash (stash@{0})
        stashes[0].0.clone()
    };

    // Get stash checkpoint
    let checkpoint_id = stashes.iter()
        .find(|(name, _)| name == &stash_name)
        .map(|(_, id)| *id)
        .ok_or_else(|| anyhow::anyhow!("Stash not found: {}", stash_name))?;

    // Restore the stash (without confirmation since it's explicit)
    crate::cmd::restore::run(&checkpoint_id.to_string(), true).await?;

    if pop {
        // Remove the stash
        pins.unpin(&stash_name)?;
        println!("{} Applied and dropped stash: {}", "✓".green(), stash_name.cyan());
    } else {
        println!("{} Applied stash: {}", "✓".green(), stash_name.cyan());
        println!("{}", "  (stash still exists, use 'tl stash drop' to remove)".dimmed());
    }

    Ok(())
}

/// Drop (delete) a stash
pub async fn run_drop(stash_ref: Option<String>) -> Result<()> {
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    // Ensure daemon is running for data access
    crate::daemon::ensure_daemon_running().await?;

    let pins = PinManager::new(&tl_dir);

    // Get all stash pins
    let all_pins = pins.list_pins()?;
    let mut stashes: Vec<(String, Ulid)> = all_pins.into_iter()
        .filter(|(name, _)| name.starts_with("stash/"))
        .collect();

    if stashes.is_empty() && stash_ref.is_none() {
        anyhow::bail!("No stashes found");
    }

    // Get checkpoint data for sorting
    let checkpoint_ids: Vec<Ulid> = stashes.iter().map(|(_, id)| *id).collect();
    let checkpoints = data_access::get_checkpoints(&checkpoint_ids, &tl_dir).await?;

    let checkpoint_map: std::collections::HashMap<Ulid, Option<Checkpoint>> =
        stashes.iter()
            .map(|(_, id)| *id)
            .zip(checkpoints.into_iter())
            .collect();

    // Sort by checkpoint timestamp (newest first)
    stashes.sort_by(|(_, id_a), (_, id_b)| {
        let cp_a = checkpoint_map.get(id_a).and_then(|o| o.as_ref());
        let cp_b = checkpoint_map.get(id_b).and_then(|o| o.as_ref());

        match (cp_a, cp_b) {
            (Some(a), Some(b)) => b.ts_unix_ms.cmp(&a.ts_unix_ms),
            _ => std::cmp::Ordering::Equal,
        }
    });

    // Resolve stash reference
    let stash_name = if let Some(ref_str) = stash_ref {
        if ref_str.starts_with("stash@{") {
            let index: usize = ref_str
                .trim_start_matches("stash@{")
                .trim_end_matches('}')
                .parse()
                .context("Invalid stash index")?;

            stashes.get(index)
                .map(|(name, _)| name.clone())
                .ok_or_else(|| anyhow::anyhow!("Stash index {} not found", index))?
        } else if ref_str.starts_with("stash/") {
            ref_str.to_string()
        } else {
            format!("stash/{}", ref_str)
        }
    } else {
        stashes.get(0)
            .map(|(name, _)| name.clone())
            .ok_or_else(|| anyhow::anyhow!("No stashes found"))?
    };

    // Remove the stash
    pins.unpin(&stash_name)?;

    println!("{} Dropped stash: {}", "✓".green(), stash_name.cyan());

    Ok(())
}

/// Clear all stashes
pub async fn run_clear(yes: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    let pins = PinManager::new(&tl_dir);

    // Get all stash pins
    let all_pins = pins.list_pins()?;
    let stashes: Vec<String> = all_pins.into_iter()
        .filter(|(name, _)| name.starts_with("stash/"))
        .map(|(name, _)| name)
        .collect();

    if stashes.is_empty() {
        println!("{}", "No stashes to clear".dimmed());
        return Ok(());
    }

    // Confirm unless -y flag is provided
    if !yes {
        println!("{}", format!("About to delete {} stashes", stashes.len()).yellow());
        println!("{}", "Use -y to confirm, or Ctrl+C to cancel".dimmed());
        anyhow::bail!("Confirmation required");
    }

    // Remove all stashes
    for stash_name in &stashes {
        pins.unpin(stash_name)?;
    }

    println!("{} Cleared {} stashes", "✓".green(), stashes.len());

    Ok(())
}
