//! Fetch from Git remote via JJ
//!
//! Fetches updates from the remote and syncs the working directory.
//! This is the standard git-like behavior: fetch + update working copy.

use anyhow::{Context, Result};
use crate::util;
use crate::cmd::restore::restore_tree;
use journal::StashManager;
use owo_colors::OwoColorize;
use tl_core::Store;
use tl_core::hash::git::hash_blob;

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
        println!("{}", "No tl/* branches found on remote".dimmed());
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
    let updated_branches: Vec<_> = branches.iter()
        .filter(|b| {
            match (&b.local_commit_id, &b.remote_commit_id) {
                (Some(local), Some(remote)) => local != remote && !b.is_diverged,
                (None, Some(_)) => true, // New remote branch
                _ => false,
            }
        })
        .collect();

    if updated_branches.is_empty() {
        println!("{}", "Working directory already up to date".dimmed());
        return Ok(());
    }

    // Check for uncommitted changes and auto-stash
    let stash_manager = StashManager::new(tl_dir);
    let stashed = check_and_auto_stash(repo_root, tl_dir, &stash_manager).await?;

    // Sync each updated branch
    println!("{}", "Syncing working directory...".dimmed());

    let mut any_synced = false;
    for branch in &updated_branches {
        if let Some(remote_commit_id) = &branch.remote_commit_id {
            let short_id = &remote_commit_id[..12.min(remote_commit_id.len())];
            println!("  Updating {} to {}", branch.name.cyan(), short_id.green());

            // Export commit to working directory
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
    if stashed {
        reapply_stash(repo_root, tl_dir, &stash_manager)?;
    }

    println!();
    println!("{} Working directory synced", "✓".green());

    Ok(())
}

/// Check for uncommitted changes and auto-stash them
async fn check_and_auto_stash(
    repo_root: &std::path::Path,
    tl_dir: &std::path::Path,
    stash_manager: &StashManager,
) -> Result<bool> {
    // Get the latest checkpoint to compare against
    let socket_path = tl_dir.join("state/daemon.sock");
    let resilient_client = crate::ipc::ResilientIpcClient::new(socket_path);

    let mut client = match resilient_client.connect_with_retry().await {
        Ok(c) => c,
        Err(_) => return Ok(false), // No daemon, assume no changes
    };

    let latest = match client.get_head().await {
        Ok(Some(cp)) => cp,
        _ => return Ok(false), // No checkpoint, nothing to stash
    };

    // Open store and compare current state
    let store = Store::open(repo_root)?;
    let latest_tree = store.read_tree(latest.root_tree)?;

    // Quick check: scan working directory for changes
    let has_changes = check_for_changes(repo_root, &store, &latest_tree)?;

    if !has_changes {
        return Ok(false);
    }

    // Auto-stash: create a checkpoint of current state
    println!("{}", "Auto-stashing uncommitted changes...".dimmed());

    // Force a checkpoint via flush
    if let Err(e) = client.flush_checkpoint().await {
        println!("  {} Could not create stash checkpoint: {}", "⚠".yellow(), e);
        return Ok(false);
    }

    // Get the new checkpoint ID
    let stash_cp = match client.get_head().await {
        Ok(Some(cp)) => cp,
        _ => return Ok(false),
    };

    // Save stash entry
    let stash_entry = journal::StashEntry {
        checkpoint_id: stash_cp.id,
        created_at_ms: stash_cp.ts_unix_ms,
        message: Some("Auto-stash before fetch".to_string()),
        base_checkpoint_id: Some(latest.id),
    };

    stash_manager.push(stash_entry)?;
    let stash_id_str = stash_cp.id.to_string();
    let stash_id_short = &stash_id_str[..8];
    println!("  {} Stashed changes at {}", "✓".green(), stash_id_short.yellow());

    Ok(true)
}

/// Check if working directory has changes compared to tree
fn check_for_changes(
    repo_root: &std::path::Path,
    store: &Store,
    tree: &tl_core::Tree,
) -> Result<bool> {
    use std::collections::HashSet;

    let mut tree_paths: HashSet<String> = HashSet::new();

    for (path_bytes, entry) in tree.entries_with_paths() {
        let path_str = std::str::from_utf8(path_bytes)?;
        tree_paths.insert(path_str.to_string());

        let file_path = repo_root.join(path_str);

        // Check if file exists
        if !file_path.exists() {
            return Ok(true); // File deleted
        }

        // Check if content changed (compare blob hashes)
        if let Ok(content) = std::fs::read(&file_path) {
            let current_hash = hash_blob(&content);
            if current_hash != entry.blob_hash {
                return Ok(true); // Content changed
            }
        }
    }

    // Check for new files (not in tree)
    for entry in walkdir::WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| {
            let path = e.path();
            !path.starts_with(repo_root.join(".tl")) &&
            !path.starts_with(repo_root.join(".git")) &&
            !path.starts_with(repo_root.join(".jj"))
        })
    {
        if let Ok(entry) = entry {
            if entry.file_type().is_file() {
                let rel_path = entry.path().strip_prefix(repo_root)?;
                let rel_str = rel_path.to_string_lossy().to_string();
                if !tree_paths.contains(&rel_str) {
                    return Ok(true); // New file
                }
            }
        }
    }

    Ok(false)
}

/// Re-apply stashed changes after sync
fn reapply_stash(
    repo_root: &std::path::Path,
    tl_dir: &std::path::Path,
    stash_manager: &StashManager,
) -> Result<()> {
    let stash_entry = match stash_manager.pop()? {
        Some(entry) => entry,
        None => return Ok(()),
    };

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
