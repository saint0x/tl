//! Add a new JJ workspace with checkpoint support

use anyhow::{anyhow, Context, Result};
use crate::util;
use journal::{Journal, PinManager};
use owo_colors::OwoColorize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tl_core::Store;

pub async fn run(
    name: &str,
    path: Option<PathBuf>,
    from: Option<String>,
    no_checkpoint: bool,
) -> Result<()> {
    // 1. Validate workspace name
    jj::validate_workspace_name(name)?;

    // 2. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 3. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 4. Generate workspace path if not provided
    let workspace_path = if let Some(p) = path {
        if p.exists() {
            anyhow::bail!(
                "Workspace path already exists: {}\n  \
                 Options:\n    \
                 - Use --path to specify a different location\n    \
                 - Remove existing directory first\n    \
                 - Use 'tl worktree switch {}' if it's already a workspace",
                p.display(),
                name
            );
        }
        p
    } else {
        generate_workspace_path(&repo_root, name)?
    };

    // 5. Open components
    let ws_manager = jj::WorkspaceManager::open(&tl_dir, &repo_root)?;
    let store = Store::open(&repo_root)?;
    let journal = Journal::open(&tl_dir.join("journal"))?;
    let pin_manager = PinManager::new(&tl_dir);

    // 6. Auto-checkpoint current workspace (unless --no-checkpoint)
    if !no_checkpoint {
        println!("{}", "Saving current workspace state...".dimmed());

        let current_name = ws_manager.current_workspace_name()?;
        let checkpoint_id = ws_manager.auto_checkpoint_current(&store, &journal)?;

        // Update current workspace state
        let current_state = jj::WorkspaceState {
            name: current_name.clone(),
            path: repo_root.clone(),
            current_checkpoint: Some(checkpoint_id),
            last_switched_ms: current_timestamp_ms(),
            created_ms: current_timestamp_ms(),
            auto_pin: Some(format!("ws-{}", current_name)),
        };
        ws_manager.set_state(&current_state)?;

        // Auto-pin
        pin_manager.pin(&format!("ws-{}", current_name), checkpoint_id)?;

        let short_id = &checkpoint_id.to_string()[..8];
        println!("  {} Checkpoint: {}", "✓".green(), short_id.yellow());
    }

    // 7. Create JJ workspace (using native jj-lib API)
    println!("{}", format!("Creating workspace '{}'...", name).dimmed());

    jj::add_workspace_native(&repo_root, name, &workspace_path)
        .context("Failed to create JJ workspace")?;

    // 8. If --from specified, restore checkpoint to new workspace
    let checkpoint_id = if let Some(from_ref) = from {
        println!("{}", "Restoring checkpoint to workspace...".dimmed());

        let checkpoint_id = util::resolve_checkpoint_ref(
            &from_ref, &journal, &pin_manager
        )?;

        ws_manager.restore_workspace_checkpoint(
            name, &store, &journal, &workspace_path
        )?;

        let short_id = &checkpoint_id.to_string()[..8];
        println!("  {} Restored checkpoint: {}", "✓".green(), short_id.yellow());

        Some(checkpoint_id)
    } else {
        None
    };

    // 9. Store workspace state
    let state = jj::WorkspaceState {
        name: name.to_string(),
        path: workspace_path.clone(),
        current_checkpoint: checkpoint_id,
        last_switched_ms: current_timestamp_ms(),
        created_ms: current_timestamp_ms(),
        auto_pin: Some(format!("ws-{}", name)),
    };
    ws_manager.set_state(&state)?;

    // 10. Success message
    println!();
    println!("{} Created workspace '{}' at {}",
        "✓".green(),
        name.cyan(),
        workspace_path.display()
    );
    println!();
    println!("  Switch with:");
    println!("    cd {}", workspace_path.display());

    Ok(())
}

/// Generate workspace path with collision detection
fn generate_workspace_path(repo_root: &Path, workspace_name: &str) -> Result<PathBuf> {
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("Cannot determine repository name"))?;

    let parent = repo_root.parent()
        .ok_or_else(|| anyhow!("Repository has no parent directory"))?;

    let mut path = parent.join(format!("{}-{}", repo_name, workspace_name));
    let mut counter = 1;

    while path.exists() {
        path = parent.join(format!("{}-{}-{}", repo_name, workspace_name, counter));
        counter += 1;
        if counter > 100 {
            anyhow::bail!("Too many workspace path collisions");
        }
    }

    Ok(path)
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before UNIX epoch")
        .as_millis() as u64
}
