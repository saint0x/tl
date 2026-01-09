//! Flush command - force checkpoint creation immediately
//!
//! With --force: Creates a proactive checkpoint even with no pending changes.
//! This is useful for marking a "restore point" before making risky changes.

use crate::ipc::IpcClient;
use crate::util;
use anyhow::{Context, Result};
use owo_colors::OwoColorize;

/// Execute flush command - force checkpoint creation
///
/// If `force` is true, creates a checkpoint even with no pending changes.
/// This is useful for creating a "restore point" before making risky changes.
pub async fn execute(force: bool) -> Result<()> {
    // Auto-start daemon if not running
    crate::daemon::ensure_daemon_running()
        .await
        .context("Daemon is required for flush command")?;

    // Find repository root
    let repo_root = util::find_repo_root()?;
    let socket_path = repo_root.join(".tl/state/daemon.sock");

    // Connect to daemon
    let mut client = IpcClient::connect(&socket_path)
        .await
        .context("Failed to connect to daemon")?;

    if force {
        // Create proactive checkpoint even with no pending changes
        match client.force_checkpoint().await {
            Ok(checkpoint_id) => {
                let short_id = &checkpoint_id[..8.min(checkpoint_id.len())];
                println!("{} Created restore point: {}",
                    "✓".green(),
                    short_id.bright_cyan()
                );
                println!("{}", "Use 'tl restore' to return to this point if needed.".dimmed());
                Ok(())
            }
            Err(e) => {
                anyhow::bail!("Failed to create restore point: {}", e)
            }
        }
    } else {
        // Normal flush - only checkpoint if there are pending changes
        match client.flush_checkpoint().await? {
            Some(checkpoint_id) => {
                let short_id = &checkpoint_id[..8.min(checkpoint_id.len())];
                println!("{} Created checkpoint: {}",
                    "✓".green(),
                    short_id.bright_cyan()
                );
                Ok(())
            }
            None => {
                println!("{}", "No pending changes to checkpoint".dimmed());
                println!("{}", "Use 'tl flush --force' to create a restore point anyway.".dimmed());
                Ok(())
            }
        }
    }
}
