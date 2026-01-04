//! Start the Timelapse daemon

use anyhow::{Context, Result};
use std::time::Duration;

pub async fn run(foreground: bool) -> Result<()> {
    if foreground {
        // Run daemon in foreground (for debugging)
        crate::daemon::start().await
    } else {
        // Start daemon in background
        start_background().await
    }
}

async fn start_background() -> Result<()> {
    use crate::util;

    let repo_root = util::find_repo_root()?;
    let log_file = repo_root.join(".tl/logs/daemon.log");

    // Use shared background start logic
    crate::daemon::start_background_internal(&repo_root).await?;

    // Wait a moment to verify it started
    tokio::time::sleep(Duration::from_millis(500)).await;

    if crate::daemon::is_running().await {
        println!("Daemon started successfully");
        println!("Logs: {}", log_file.display());
        Ok(())
    } else {
        anyhow::bail!(
            "Daemon failed to start (check logs at {})",
            log_file.display()
        );
    }
}
