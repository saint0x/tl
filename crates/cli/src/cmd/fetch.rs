//! Fetch from Git remote via JJ
//!
//! Fetches updates from the remote and syncs the working directory.
//! This is the standard git-like behavior: fetch + update working copy.

use anyhow::{Context, Result};
use crate::util;
use crate::cmd::restore::restore_tree;
use journal::{StashEntry, StashManager};
use owo_colors::OwoColorize;
use tl_core::Store;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn run(no_sync: bool, prune: bool) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    // `tl fetch --no-sync` should be strict Git parity (fetch only).
    if no_sync {
        println!("{}", "Fetching from remote...".dimmed());
        util::git_fetch_origin(&repo_root, prune)?;
        println!("{} Fetch complete", "✓".green());
        return Ok(());
    }

    // 2. Verify JJ workspace exists (required for sync / branch display)
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // We will touch the working directory (auto-stash, sync, pathmap invalidation).
    crate::daemon::ensure_daemon_running().await?;

    // 3. Load workspace
    println!("{}", "Fetching from remote...".dimmed());
    let mut workspace = jj::load_workspace(&repo_root)
        .context("Failed to load JJ workspace")?;

    // 4. Perform native git fetch via JJ (imports refs, updates view)
    jj::git_ops::native_git_fetch(&mut workspace)?;
    println!("{} Fetch complete", "✓".green());

    // 5. Show what was fetched (fast path: avoid expensive ahead/behind counts)
    let branches = jj::git_ops::get_remote_branch_updates_fast(&workspace)?;

    if !branches.is_empty() {
        println!();
        println!("Remote branches:");
        for branch in &branches {
            let remote_id = branch.remote_commit_id.as_ref()
                .map(|s| &s[..12.min(s.len())])
                .unwrap_or("???");

            let status = if let Some(local) = &branch.local_commit_id {
                if local == branch.remote_commit_id.as_ref().unwrap_or(&String::new()) {
                    "up to date".dimmed().to_string()
                } else if branch.is_diverged {
                    "diverged".red().to_string()
                } else {
                    "updated".green().to_string()
                }
            } else {
                "new".cyan().to_string()
            };

            println!("  {} {} ({})", branch.name.cyan(), remote_id.dimmed(), status);
        }
    } else {
        println!("{}", "No remote branches found".dimmed());
    }

    // 6. Handle prune flag
    if prune {
        println!();
        println!("{}", "Pruning deleted remote branches...".dimmed());
        println!("{}", "Prune complete".dimmed());
    }

    // 7. Sync working directory (unless --no-sync)
    if !no_sync {
        println!();
        sync_working_directory(&repo_root, &tl_dir, &workspace, &branches).await?;
    } else {
        println!();
        println!("{}", "Skipping working directory sync (--no-sync)".dimmed());
    }

    Ok(())
}

/// Sync working directory to match remote state
async fn sync_working_directory(
    repo_root: &std::path::Path,
    tl_dir: &std::path::Path,
    workspace: &jj_lib::workspace::Workspace,
    branches: &[jj::RemoteBranchInfo],
) -> Result<()> {
    // Find branches that have updates (remote differs from local)
    let (_remote_name, branch_name) = util::resolve_git_sync_target(repo_root)?;
    let sync_branch = match branches.iter().find(|b| b.name == branch_name) {
        Some(branch) => branch,
        None => {
            println!(
                "{}",
                format!(
                    "Remote branch '{}' is not present; skipping working directory sync.",
                    branch_name
                ).dimmed()
            );
            return Ok(());
        }
    };

    let branch_needs_sync = match (&sync_branch.local_commit_id, &sync_branch.remote_commit_id) {
        (Some(local), Some(remote)) => local != remote && !sync_branch.is_diverged,
        (None, Some(_)) => true,
        _ => false,
    };

    if !branch_needs_sync {
        println!("{}", "Working directory already up to date".dimmed());
        return Ok(());
    }

    // Check for uncommitted changes and auto-stash
    let stash_manager = StashManager::new(tl_dir);
    let stash_entry = check_and_auto_stash(tl_dir, &stash_manager).await?;

    // Sync each updated branch
    println!("{}", "Syncing working directory...".dimmed());

    let mut any_synced = false;
    if let Some(remote_commit_id) = &sync_branch.remote_commit_id {
        let short_id = &remote_commit_id[..12.min(remote_commit_id.len())];
        println!("  Updating {} to {}", sync_branch.name.cyan(), short_id.green());

        match jj::git_ops::export_commit_to_dir(workspace, remote_commit_id, repo_root) {
            Ok(()) => {
                println!("    {} Files synced", "✓".green());
                any_synced = true;
            }
            Err(e) => {
                println!("    {} Failed to sync: {}", "✗".red(), e);
            }
        }
    }

    // CRITICAL: Invalidate pathmap after modifying working directory
    // The daemon's in-memory pathmap is now stale and needs to be rebuilt
    if any_synced {
        let socket_path = tl_dir.join("state/daemon.sock");
        if socket_path.exists() {
            if let Ok(mut client) = crate::ipc::IpcClient::connect(&socket_path).await {
                if let Err(e) = client.invalidate_pathmap().await {
                    tracing::warn!("Failed to invalidate pathmap after fetch sync: {}", e);
                }
            }
        }
    }

    // Re-apply stash if we stashed changes
    if let Some(stash) = stash_entry {
        reapply_stash(repo_root, tl_dir, &stash_manager, stash)?;
    }

    println!();
    println!("{} Working directory synced", "✓".green());

    Ok(())
}

/// Check for uncommitted changes and auto-stash them
async fn check_and_auto_stash(
    tl_dir: &std::path::Path,
    stash_manager: &StashManager,
) -> Result<Option<StashEntry>> {
    let socket_path = tl_dir.join("state/daemon.sock");
    let resilient = crate::ipc::ResilientIpcClient::new(socket_path);

    let base = match resilient
        .send_request_resilient(&crate::ipc::IpcRequest::GetHead)
        .await?
    {
        crate::ipc::IpcResponse::Head(cp) => cp,
        crate::ipc::IpcResponse::Error(e) => anyhow::bail!("Daemon error: {}", e),
        _ => anyhow::bail!("Unexpected response to GetHead"),
    };
    let base_id = base.as_ref().map(|cp| cp.id);

    let mut sleep_ms = 50u64;
    for _ in 0..5 {
        match resilient
            .send_request_resilient(&crate::ipc::IpcRequest::FlushCheckpointFast)
            .await?
        {
            crate::ipc::IpcResponse::CheckpointFlushed(Some(id_str)) => {
                let checkpoint_id = ulid::Ulid::from_string(&id_str)?;
                println!("{}", "Auto-stashing uncommitted changes...".dimmed());

                let entry = StashEntry {
                    checkpoint_id,
                    created_at_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)?
                        .as_millis() as u64,
                    message: Some("Auto-stash before fetch".to_string()),
                    base_checkpoint_id: base_id,
                };

                stash_manager.push(entry.clone())?;
                let short_stash_id = &checkpoint_id.to_string()[..8];
                println!("  {} Stashed changes at {}", "✓".green(), short_stash_id.yellow());

                return Ok(Some(entry));
            }
            crate::ipc::IpcResponse::CheckpointFlushed(None) => {
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                sleep_ms = (sleep_ms * 2).min(400);
            }
            crate::ipc::IpcResponse::Error(e) => anyhow::bail!("Daemon error: {}", e),
            _ => anyhow::bail!("Unexpected response to FlushCheckpointFast"),
        }
    }

    Ok(None)
}

/// Re-apply stashed changes after sync
fn reapply_stash(
    repo_root: &std::path::Path,
    tl_dir: &std::path::Path,
    stash_manager: &StashManager,
    stash_entry: StashEntry,
) -> Result<()> {
    match stash_manager.pop()? {
        Some(entry) if entry.checkpoint_id == stash_entry.checkpoint_id => {}
        Some(_) => anyhow::bail!("Stash stack changed during fetch"),
        None => return Ok(()),
    }

    println!();
    println!("{}", "Re-applying stashed changes...".dimmed());

    // Load the stashed checkpoint's tree
    let store = Store::open(repo_root)?;

    // Get checkpoint data via data access layer
    let checkpoints = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            crate::data_access::get_checkpoints(&[stash_entry.checkpoint_id], tl_dir).await
        })
    })?;

    let stash_cp = match checkpoints.get(0).and_then(|c| c.as_ref()) {
        Some(cp) => cp,
        None => {
            println!("  {} Stash checkpoint not found", "⚠".yellow());
            return Ok(());
        }
    };

    let stash_tree = store.read_tree(stash_cp.root_tree)?;

    // Restore stashed files (without deleting extra files)
    let result = restore_tree(&store, &stash_tree, repo_root, false)?;

    if result.errors.is_empty() {
        println!("  {} Re-applied {} files", "✓".green(), result.files_restored);
    } else {
        println!("  {} Re-applied {} files with {} errors",
            "⚠".yellow(), result.files_restored, result.errors.len());
        for error in result.errors.iter().take(3) {
            println!("    {}", error.red());
        }
    }

    Ok(())
}
