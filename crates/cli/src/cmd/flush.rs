//! Flush command - force checkpoint creation immediately

use crate::ipc::IpcClient;
use crate::util;
use anyhow::{Context, Result};

/// Execute flush command - force checkpoint creation
pub async fn execute() -> Result<()> {
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

    // Request flush
    match client.flush_checkpoint().await? {
        Some(checkpoint_id) => {
            println!("Created checkpoint: {}", checkpoint_id);
            Ok(())
        }
        None => {
            println!("No pending changes to checkpoint");
            Ok(())
        }
    }
}
