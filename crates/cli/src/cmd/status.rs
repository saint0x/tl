//! Show daemon and checkpoint status

use crate::util;
use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn run(show_remote: bool) -> Result<()> {
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

    // Remote status (if requested)
    if show_remote {
        print_remote_status(&repo_root)?;
    }

    Ok(())
}

/// Get the git remote URL for "origin"
fn get_git_remote_url(repo_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
        .ok()?;

    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

/// Print remote branch status
fn print_remote_status(repo_root: &Path) -> Result<()> {
    // Check if JJ workspace exists
    if jj::detect_jj_workspace(repo_root)?.is_none() {
        println!("{}", "Remote: No JJ workspace (run 'tl init' first)".dimmed());
        return Ok(());
    }

    // Get remote URL
    let remote_url = get_git_remote_url(repo_root);

    println!("{}", "Remote Status".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    match &remote_url {
        Some(url) => println!("Remote:  {} ({})", "origin".cyan(), url.dimmed()),
        None => {
            println!("Remote:  {} {}", "origin".cyan(), "(not configured)".yellow());
            println!();
            println!("{}", "No git remote configured. Add one with:".dimmed());
            println!("  git remote add origin <url>");
            return Ok(());
        }
    }
    println!();

    // Load workspace and get branch status
    let workspace = match jj::load_workspace(repo_root) {
        Ok(ws) => ws,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return Ok(());
        }
    };

    let branches = match jj::git_ops::get_remote_branch_updates(&workspace) {
        Ok(b) => b,
        Err(e) => {
            println!("{} {}", "Error fetching remote status:".red(), e);
            return Ok(());
        }
    };

    if branches.is_empty() {
        println!("  {}", "No local branches tracked".dimmed());
        println!();
        println!("{}", "Publish a checkpoint first:".dimmed());
        println!("  tl publish HEAD");
        return Ok(());
    }

    println!("Branches:");
    for branch in &branches {
        let status = format_branch_status(branch);
        let local_id = branch.local_commit_id.as_ref()
            .map(|s| &s[..12.min(s.len())])
            .unwrap_or("(none)");

        println!("  {} {} {}",
            branch.name.cyan(),
            local_id.dimmed(),
            status);
    }
    println!();

    Ok(())
}

/// Format branch status as colored string
fn format_branch_status(branch: &jj::RemoteBranchInfo) -> String {
    use owo_colors::OwoColorize;

    match (&branch.local_commit_id, &branch.remote_commit_id) {
        (Some(local), Some(remote)) => {
            if local == remote {
                "✓ up to date".green().to_string()
            } else if branch.is_diverged {
                format!("{} {} {}",
                    "↑".yellow(),
                    "↓".yellow(),
                    "diverged".red().bold())
            } else if branch.commits_ahead > 0 {
                format!("{}{} {}",
                    "↑".green(),
                    branch.commits_ahead,
                    "ahead".green())
            } else if branch.commits_behind > 0 {
                format!("{}{} {}",
                    "↓".yellow(),
                    branch.commits_behind,
                    "behind".yellow())
            } else {
                // Different commits but we don't know ahead/behind count yet
                format!("{}", "differs from remote".yellow())
            }
        }
        (Some(_), None) => {
            format!("{} {}", "↑".green(), "local only".cyan())
        }
        (None, Some(_)) => {
            format!("{} {}", "↓".yellow(), "remote only".yellow())
        }
        (None, None) => {
            "(unknown)".dimmed().to_string()
        }
    }
}
