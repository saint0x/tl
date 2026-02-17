//! Pull from Git remote with auto-stash and working directory sync
//!
//! Full workflow:
//! 1. Check for uncommitted changes
//! 2. Auto-stash if changes exist
//! 3. Fetch from remote
//! 4. Import remote commits as checkpoints
//! 5. Sync working directory to remote HEAD
//! 6. Re-apply stash (if any)

use anyhow::{Context, Result};
use crate::util;
use crate::cmd::restore::restore_tree;
use journal::{Checkpoint, StashEntry, StashManager};
use owo_colors::OwoColorize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tl_core::Store;

pub async fn run(
    fetch_only: bool,
    no_pin: bool,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Fetch from remote using latency-optimized refspec selection
    //
    // Fetch-only mode should not require the daemon at all; it should be as close
    // to `jj git fetch`/`git fetch` as possible.
    println!("{}", "Fetching from Git remote...".dimmed());
    let mut workspace = jj::load_workspace(&repo_root)
        .context("Failed to load JJ workspace")?;

    jj::git_ops::native_git_fetch_for_pull(&mut workspace)?;
    println!("{} Fetched from remote", "✓".green());

    // If fetch-only mode, stop here
    if fetch_only {
        println!();
        println!("{}", "Fetch complete. Use 'tl pull' (without --fetch-only) to sync working directory.".dimmed());
        return Ok(());
    }

    // From here on we need the daemon (checkpoint import, flush-based auto-stash, etc.).
    crate::daemon::ensure_daemon_running().await?;

    // 4. Open components
    let store = Store::open(&repo_root)
        .context("Failed to open Timelapse store")?;
    let stash_manager = StashManager::new(&tl_dir);

    // 5. Check for uncommitted changes and auto-stash
    let stash_entry = check_and_auto_stash(&tl_dir, &stash_manager).await?;

    // 6. Choose sync target quickly (prefer main/master, else first remote bookmark)
    let (_sync_branch_name, remote_commit_id) =
        match jj::git_ops::get_preferred_remote_head(&workspace, &["main", "master"])? {
            Some(v) => v,
            None => {
                println!("{}", "No branches found on remote.".dimmed());
                if let Some(stash) = stash_entry {
                    reapply_stash(&repo_root, &tl_dir, &store, &stash_manager, stash).await?;
                }
                return Ok(());
            }
        };

    // 7. Import remote commit as checkpoint via the daemon (journal is daemon-owned).
    let imported = import_remote_commit_via_daemon(&tl_dir, &remote_commit_id, no_pin).await?;

    if imported.already_present {
        let short_id = &imported.checkpoint.id.to_string()[..8];
        println!(
            "{} Already up to date (checkpoint {})",
            "✓".green(),
            short_id.bright_cyan()
        );
    } else {
        println!();
        println!("{}", "Importing remote changes...".dimmed());

        let short_commit = &remote_commit_id[..12.min(remote_commit_id.len())];
        let short_cp_id = &imported.checkpoint.id.to_string()[..8];
        println!(
            "{} Imported {} as checkpoint {}",
            "✓".green(),
            short_commit.bright_yellow(),
            short_cp_id.bright_cyan()
        );

        // 8. Sync working directory to imported checkpoint
        // Note: delete_extra=false to preserve local files (Git-like behavior)
        println!();
        println!("{}", "Syncing working directory...".dimmed());

        let tree = store.read_tree(imported.checkpoint.root_tree)?;
        let result = restore_tree(&store, &tree, &repo_root, false)?;

        println!(
            "{} Synced {} files",
            "✓".green(),
            result.files_restored.to_string().green()
        );
        if !result.errors.is_empty() {
            println!("{} {} errors during sync", "!".yellow(), result.errors.len());
            for err in result.errors.iter().take(3) {
                println!("  {}", err.dimmed());
            }
        }

        // CRITICAL (Fix 12): Invalidate pathmap after modifying working directory
        // The daemon's in-memory pathmap is now stale and needs to be rebuilt
        let socket_path = tl_dir.join("state/daemon.sock");
        if socket_path.exists() {
            if let Ok(mut client) = crate::ipc::IpcClient::connect(&socket_path).await {
                if let Err(e) = client.invalidate_pathmap().await {
                    tracing::warn!("Failed to invalidate pathmap after pull: {}", e);
                }
            }
        }
    }

    // 10. Re-apply stash if we had uncommitted changes
    if let Some(stash) = stash_entry {
        reapply_stash(&repo_root, &tl_dir, &store, &stash_manager, stash).await?;
    }

    println!();
    println!("{}", "Pull complete.".dimmed());

    Ok(())
}

/// Check for uncommitted changes and auto-stash if found.
///
/// Production behavior:
/// - Never does a full filesystem scan in the CLI (latency).
/// - Instead, asks the daemon to flush pending watcher events into a checkpoint.
/// - Retries briefly to cover watcher latency windows.
async fn check_and_auto_stash(
    tl_dir: &Path,
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

    // Retry to cover watcher/event latency (e.g. atomic saves).
    let mut sleep_ms = 50u64;
    for _ in 0..5 {
        match resilient
            .send_request_resilient(&crate::ipc::IpcRequest::FlushCheckpointFast)
            .await?
        {
            crate::ipc::IpcResponse::CheckpointFlushed(Some(id_str)) => {
                let checkpoint_id = ulid::Ulid::from_string(&id_str)?;

                println!("{}", "Auto-stashing uncommitted changes...".dimmed());
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_millis() as u64;

                let entry = StashEntry {
                    checkpoint_id,
                    created_at_ms: now_ms,
                    message: Some("Auto-stash before pull".to_string()),
                    base_checkpoint_id: base_id,
                };

                stash_manager.push(entry.clone())?;

                let short_stash_id = &checkpoint_id.to_string()[..8];
                println!(
                    "{} Stashed changes in checkpoint {}",
                    "✓".green(),
                    short_stash_id.bright_cyan()
                );

                return Ok(Some(entry));
            }
            crate::ipc::IpcResponse::CheckpointFlushed(None) => {
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                sleep_ms = (sleep_ms * 2).min(400);
            }
            crate::ipc::IpcResponse::Error(e) => anyhow::bail!("Daemon error: {}", e),
            _ => anyhow::bail!("Unexpected response to FlushCheckpoint"),
        }
    }

    Ok(None)
}

struct ImportResult {
    checkpoint: Checkpoint,
    already_present: bool,
}

async fn import_remote_commit_via_daemon(
    tl_dir: &Path,
    commit_id: &str,
    no_pin: bool,
) -> Result<ImportResult> {
    let socket_path = tl_dir.join("state/daemon.sock");
    let resilient = crate::ipc::ResilientIpcClient::new(socket_path);

    match resilient
        .send_request_resilient(&crate::ipc::IpcRequest::ImportRemoteCommit {
            commit_id: commit_id.to_string(),
            no_pin,
        })
        .await?
    {
        crate::ipc::IpcResponse::ImportedCommit {
            checkpoint,
            already_present,
        } => Ok(ImportResult {
            checkpoint,
            already_present,
        }),
        crate::ipc::IpcResponse::Error(e) => anyhow::bail!("Daemon error: {}", e),
        other => anyhow::bail!("Unexpected response to ImportRemoteCommit: {:?}", other),
    }
}

/// Re-apply stashed changes after pull
async fn reapply_stash(
    repo_root: &Path,
    tl_dir: &Path,
    store: &Store,
    stash_manager: &StashManager,
    stash: StashEntry,
) -> Result<()> {
    println!();
    println!("{}", "Re-applying stashed changes...".dimmed());

    // Get checkpoints via daemon IPC (journal is daemon-owned).
    let socket_path = tl_dir.join("state/daemon.sock");
    let resilient = crate::ipc::ResilientIpcClient::new(socket_path);

    let stash_checkpoint = match resilient
        .send_request_resilient(&crate::ipc::IpcRequest::GetCheckpoint(stash.checkpoint_id.to_string()))
        .await?
    {
        crate::ipc::IpcResponse::Checkpoint(Some(cp)) => cp,
        crate::ipc::IpcResponse::Checkpoint(None) => anyhow::bail!("Stash checkpoint not found"),
        crate::ipc::IpcResponse::Error(e) => anyhow::bail!("Daemon error: {}", e),
        _ => anyhow::bail!("Unexpected response to GetCheckpoint"),
    };

    let current = match resilient
        .send_request_resilient(&crate::ipc::IpcRequest::GetHead)
        .await?
    {
        crate::ipc::IpcResponse::Head(Some(cp)) => cp,
        crate::ipc::IpcResponse::Head(None) => anyhow::bail!("No current checkpoint"),
        crate::ipc::IpcResponse::Error(e) => anyhow::bail!("Daemon error: {}", e),
        _ => anyhow::bail!("Unexpected response to GetHead"),
    };

    // Simple approach: restore stash on top of current
    // TODO: Implement proper 3-way merge for conflicts
    let stash_tree = store.read_tree(stash_checkpoint.root_tree)?;
    let current_tree = store.read_tree(current.root_tree)?;

    // Find files that were changed in stash but not in remote
    let mut reapplied = 0;
    let mut conflicts = Vec::new();

    for (path_bytes, stash_entry) in stash_tree.entries_with_paths() {
        let path_str = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Skip protected paths
        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") || path_str.starts_with(".jj/") {
            continue;
        }

        let file_path = repo_root.join(path_str);

        // Check if current (pulled) state has this file
        if let Some(current_entry) = current_tree.get(std::path::Path::new(path_str)) {
            // Both have the file - check for conflict
            if stash_entry.blob_hash != current_entry.blob_hash {
                // File differs between stash and pulled - potential conflict
                // For now, stash wins (user's local changes take precedence)
                conflicts.push(path_str.to_string());
            }
        }

        // Apply stash entry
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = store.blob_store().read_blob(stash_entry.blob_hash)?;
        std::fs::write(&file_path, content)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(stash_entry.mode);
            std::fs::set_permissions(&file_path, permissions)?;
        }

        reapplied += 1;
    }

    // Print success messages BEFORE removing stash
    // This ensures if anything fails, the stash is preserved for retry
    println!("{} Re-applied {} files from stash", "✓".green(), reapplied.to_string().green());

    if !conflicts.is_empty() {
        println!("{} {} files had both local and remote changes (local wins):",
            "!".yellow(), conflicts.len());
        for path in conflicts.iter().take(5) {
            println!("  {}", path.dimmed());
        }
        if conflicts.len() > 5 {
            println!("  ... and {} more", conflicts.len() - 5);
        }
    }

    // Remove the stash ONLY after all operations complete successfully
    // CRITICAL: This must be the last operation to prevent data loss
    // If pop() fails, stash is preserved and user can retry
    stash_manager.pop()?;

    Ok(())
}

// NOTE: The old tempdir export + WalkDir re-import path was removed for latency reasons.
