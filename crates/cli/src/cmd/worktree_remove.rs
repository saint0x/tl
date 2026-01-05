//! Remove a JJ workspace with cleanup

use anyhow::{anyhow, Context, Result};
use crate::util;
use journal::PinManager;
use owo_colors::OwoColorize;
use std::fs;
use std::io::{self, Write};
use std::process::Command;

pub async fn run(name: &str, delete_files: bool, yes: bool) -> Result<()> {
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
    let pin_manager = PinManager::new(&tl_dir);

    // 4. Validate not removing current workspace
    let current_name = ws_manager.current_workspace_name()?;
    if current_name == name {
        // Get available workspaces for helpful message
        let other_workspaces: Vec<_> = ws_manager.list_jj_workspaces()?
            .iter()
            .filter(|w| w.name != name)
            .map(|w| w.name.clone())
            .collect();

        if other_workspaces.is_empty() {
            anyhow::bail!(
                "Cannot remove current workspace.\n  \
                 This is the only workspace."
            );
        } else {
            anyhow::bail!(
                "Cannot remove current workspace.\n  \
                 Switch to another workspace first:\n{}",
                other_workspaces.iter()
                    .map(|n| format!("    tl worktree switch {}", n))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    // 5. Get workspace state (to find path)
    let state = ws_manager.get_state(name)?
        .ok_or_else(|| anyhow!("Workspace '{}' not found", name))?;

    // 6. Run jj workspace forget (using native jj-lib API)
    println!("{}", format!("Removing workspace '{}' from JJ tracking...", name).dimmed());

    jj::forget_workspace_native(&repo_root, name)
        .context("Failed to forget JJ workspace")?;

    println!("  {} Removed from JJ tracking", "✓".green());

    // 7. If --delete-files, confirm and delete directory
    if delete_files {
        let should_delete = if yes {
            true
        } else {
            println!();
            println!("{}", "⚠️  Warning: This will delete all files in the workspace!".red().bold());
            println!("  Path: {}", state.path.display());
            print!("  Continue? [y/N] ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input.trim().eq_ignore_ascii_case("y")
        };

        if should_delete {
            println!();
            println!("{}", "Deleting workspace files...".dimmed());

            fs::remove_dir_all(&state.path)
                .with_context(|| format!("Failed to delete {}", state.path.display()))?;

            println!("  {} Deleted workspace files", "✓".green());
        } else {
            println!();
            println!("{}", "Cancelled file deletion".dimmed());
        }
    }

    // 8. Clean up timelapse metadata
    println!();
    println!("{}", "Cleaning up timelapse metadata...".dimmed());

    ws_manager.delete_state(name)?;

    // Unpin auto-pin if exists
    if let Some(auto_pin) = state.auto_pin {
        pin_manager.unpin(&auto_pin).ok(); // Ignore errors
    }

    println!("  {} Cleaned up metadata", "✓".green());

    // 9. Success message
    println!();
    println!("{} Removed workspace '{}'", "✓".green(), name.cyan());

    if !delete_files {
        println!();
        println!("  Note: Files at {} preserved", state.path.display());
        println!("        (use --delete-files to remove)");
    }

    Ok(())
}
