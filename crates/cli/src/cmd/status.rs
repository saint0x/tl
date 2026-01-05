//! Show daemon and checkpoint status

use crate::util;
use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn run() -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Ensure daemon is running (auto-start with supervisor)
    crate::daemon::ensure_daemon_running().await?;

    // 3. Connect to daemon with retry
    let socket_path = tl_dir.join("state/daemon.sock");
    let resilient_client = crate::ipc::ResilientIpcClient::new(socket_path);
    let mut client = resilient_client.connect_with_retry().await
        .context("Failed to connect to daemon")?;

    // 4. Get all data via single batched IPC call
    let (status, latest, checkpoint_count) = client.get_status_full().await
        .context("Failed to retrieve status from daemon")?;

    // 5. Get storage stats
    let total_size = util::calculate_dir_size(&tl_dir)?;

    // 5. Display output
    println!("{}", "Repository Status".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    // Repository path
    println!("Repository:    {}", repo_root.display().to_string().cyan());
    println!();

    // Daemon status (always running since we connected successfully)
    println!("Daemon:        {}", "Running ✓".green());
    println!("  PID:         {}", status.pid);

    // Calculate uptime from daemon start time
    let current_time_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let uptime_secs = (current_time_ms.saturating_sub(status.start_time_ms)) / 1000;
    println!("  Uptime:      {}", util::format_duration(uptime_secs));
    println!("  Checkpoints: {} created", status.checkpoints_created);
    if let Some(ts) = status.last_checkpoint_time {
        println!("  Last:        {}", util::format_relative_time(ts));
    }
    println!("  Watching:    {} paths", status.watcher_paths);
    println!();

    // Latest checkpoint
    println!("Latest checkpoint:");
    if let Some(cp) = latest {
        let id_short = cp.id.to_string()[..8].to_string();
        let time_str = util::format_relative_time(cp.ts_unix_ms);
        let abs_time = util::format_absolute_time(cp.ts_unix_ms);

        println!("  ID:          {}", id_short.yellow());
        println!("  Time:        {} ({})", time_str, abs_time.dimmed());
        println!("  Files:       {}", cp.meta.files_changed);
        println!("  Reason:      {:?}", cp.reason);

        if !cp.touched_paths.is_empty() {
            println!("  Changed:");
            for path in cp.touched_paths.iter().take(5) {
                println!("    - {}", path.display());
            }
            if cp.touched_paths.len() > 5 {
                println!("    ... and {} more", cp.touched_paths.len() - 5);
            }
        }
    } else {
        println!("  {}", "No checkpoints yet".dimmed());
    }
    println!();

    // Storage summary
    println!("Storage:");
    println!("  Checkpoints: {}", checkpoint_count);
    println!("  Total size:  {}", util::format_size(total_size));
    println!();

    Ok(())
}
