//! List all JJ workspaces with status

use anyhow::{Context, Result};
use crate::util;
use owo_colors::OwoColorize;

pub async fn run() -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Open components
    let ws_manager = jj::WorkspaceManager::open(&tl_dir, &repo_root)?;

    // 4. Get JJ workspaces
    let workspaces = ws_manager.list_jj_workspaces()?;

    if workspaces.is_empty() {
        println!("{}", "No workspaces found.".dimmed());
        println!("{}", "Create one with: tl worktree add <name>".dimmed());
        return Ok(());
    }

    // 5. Display table header
    println!("{}", "Workspaces".bold());
    println!("{}", "━".repeat(90));
    println!("{:<18} {:<30} {:<14} {}",
        "NAME", "PATH", "CHECKPOINT", "STATUS"
    );
    println!("{}", "─".repeat(90));

    // 6. Display each workspace
    for ws in workspaces {
        let state = ws_manager.get_state(&ws.name)?;

        // Format checkpoint info
        let checkpoint_str = if let Some(ref state) = state {
            if let Some(cp_id) = state.current_checkpoint {
                format!("{}...", &cp_id.to_string()[..8]).yellow().to_string()
            } else {
                "-".dimmed().to_string()
            }
        } else {
            "-".dimmed().to_string()
        };

        // Format status
        let status_str = if ws.is_current && ws.has_changes {
            "Working".cyan().bold().to_string()
        } else if ws.is_current {
            "Current".cyan().to_string()
        } else if ws.has_changes {
            "Modified".yellow().to_string()
        } else {
            "Clean".green().to_string()
        };

        // Format name (highlight current)
        let name_str = if ws.is_current {
            format!("{} (current)", ws.name).cyan().to_string()
        } else {
            ws.name.clone()
        };

        // Format path (truncate if too long)
        let path_str = ws.path.to_string_lossy();
        let path_display = if path_str.len() > 30 {
            format!("...{}", &path_str[path_str.len()-27..])
        } else {
            path_str.to_string()
        };

        println!("{:<18} {:<30} {:<14} {}",
            name_str,
            path_display,
            checkpoint_str,
            status_str
        );
    }

    println!();
    println!("{}", "Use 'tl worktree switch <name>' to switch workspaces".dimmed());

    Ok(())
}
