//! Switch to a different JJ workspace with auto-checkpoint

use anyhow::{anyhow, Context, Result};
use crate::util;
use journal::{Journal, PinManager};
use owo_colors::OwoColorize;
use std::time::{SystemTime, UNIX_EPOCH};
use tl_core::Store;

pub async fn run(name: &str) -> Result<()> {
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
    let store = Store::open(&repo_root)?;
    let journal = Journal::open(&tl_dir.join("journal"))?;
    let pin_manager = PinManager::new(&tl_dir);

    // 4. Get current workspace name
    let current_name = ws_manager.current_workspace_name()?;

    // 5. If switching to same workspace, no-op
    if current_name == name {
        println!("{}", format!("Already on workspace '{}'", name).dimmed());
        return Ok(());
    }

    // 6. Verify target workspace exists
    let workspaces = ws_manager.list_jj_workspaces()?;
    let target_ws = workspaces.iter()
        .find(|w| w.name == name)
        .ok_or_else(|| {
            let available: Vec<_> = workspaces.iter().map(|w| w.name.clone()).collect();
            anyhow!(
                "Workspace '{}' not found.\n  Available: {}",
                name,
                available.join(", ")
            )
        })?;

    // 7. Auto-checkpoint current workspace
    println!("{}", "Saving current workspace state...".dimmed());

    let checkpoint_id = ws_manager.auto_checkpoint_current(&store, &journal)?;

    // Update current workspace state
    let current_state = ws_manager.get_state(&current_name)?
        .unwrap_or_else(|| jj::WorkspaceState {
            name: current_name.clone(),
            path: repo_root.clone(),
            current_checkpoint: None,
            last_switched_ms: 0,
            created_ms: current_timestamp_ms(),
            auto_pin: None,
        });

    let updated_state = jj::WorkspaceState {
        current_checkpoint: Some(checkpoint_id),
        last_switched_ms: current_timestamp_ms(),
        auto_pin: Some(format!("ws:{}", current_name)),
        ..current_state
    };
    ws_manager.set_state(&updated_state)?;

    // Auto-pin for easy reference
    pin_manager.pin(&format!("ws:{}", current_name), checkpoint_id)?;

    let short_id = &checkpoint_id.to_string()[..8];
    println!("  {} Checkpoint: {}", "✓".green(), short_id.yellow());

    // 8. Restore target workspace checkpoint (if exists)
    if let Some(target_state) = ws_manager.get_state(name)? {
        if let Some(target_checkpoint_id) = target_state.current_checkpoint {
            println!();
            println!("{}", "Restoring workspace state...".dimmed());

            ws_manager.restore_workspace_checkpoint(
                name, &store, &journal, &target_ws.path
            )?;

            let short_id = &target_checkpoint_id.to_string()[..8];
            println!("  {} Restored checkpoint: {}", "✓".green(), short_id.yellow());
        }
    }

    // 9. Success message
    println!();
    println!("{} Switched to workspace '{}'",
        "✓".green(),
        name.cyan()
    );
    println!();
    println!("  Change directory with:");
    println!("    cd {}", target_ws.path.display());

    Ok(())
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before UNIX epoch")
        .as_millis() as u64
}
