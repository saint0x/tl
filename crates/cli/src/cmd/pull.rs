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
use crate::cmd::restore::{restore_tree, RestoreResult};
use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal, StashEntry, StashManager};
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

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Open components
    let store = Store::open(&repo_root)
        .context("Failed to open Timelapse store")?;
    let journal = Journal::open(&tl_dir)
        .context("Failed to open journal")?;
    let stash_manager = StashManager::new(&tl_dir);
    let mapping = jj::JjMapping::open(&tl_dir)
        .context("Failed to open JJ mapping")?;

    // 4. Check for uncommitted changes and auto-stash
    let stash_entry = if !fetch_only {
        check_and_auto_stash(&repo_root, &store, &journal, &stash_manager).await?
    } else {
        None
    };

    // 5. Fetch from remote using native git API
    println!("{}", "Fetching from Git remote...".dimmed());
    let mut workspace = jj::load_workspace(&repo_root)
        .context("Failed to load JJ workspace")?;

    jj::git_ops::native_git_fetch(&mut workspace)?;
    println!("{} Fetched from remote", "✓".green());

    // If fetch-only mode, stop here
    if fetch_only {
        println!();
        println!("{}", "Fetch complete. Use 'tl pull' (without --fetch-only) to sync working directory.".dimmed());
        return Ok(());
    }

    // 6. Get remote branch updates
    let remote_branches = jj::git_ops::get_remote_branch_updates(&workspace)?;

    // Find the primary branch to sync to (prefer main, then master, then first available)
    let sync_branch = remote_branches.iter()
        .find(|b| b.name == "main")
        .or_else(|| remote_branches.iter().find(|b| b.name == "master"))
        .or_else(|| remote_branches.first());

    let sync_branch = match sync_branch {
        Some(b) => b,
        None => {
            println!("{}", "No branches found on remote.".dimmed());
            // Try to re-apply stash even if no remote branches
            if let Some(stash) = stash_entry {
                reapply_stash(&repo_root, &store, &journal, &stash_manager, stash).await?;
            }
            return Ok(());
        }
    };

    let remote_commit_id = match &sync_branch.remote_commit_id {
        Some(id) => id.clone(),
        None => {
            println!("{} Remote branch {} has no commit", "!".yellow(), sync_branch.name);
            if let Some(stash) = stash_entry {
                reapply_stash(&repo_root, &store, &journal, &stash_manager, stash).await?;
            }
            return Ok(());
        }
    };

    // 7. Check if already imported
    if let Some(checkpoint_id) = mapping.get_checkpoint(&remote_commit_id)? {
        let short_id = &checkpoint_id.to_string()[..8];
        println!("{} Already up to date (checkpoint {})", "✓".green(),
            short_id.bright_cyan());
    } else {
        // 8. Import remote commit as checkpoint
        println!();
        println!("{}", "Importing remote changes...".dimmed());

        let checkpoint = import_remote_commit(
            &workspace,
            &remote_commit_id,
            &store,
            &journal,
            &mapping,
            no_pin,
            &tl_dir,
        )?;

        let short_commit = &remote_commit_id[..12.min(remote_commit_id.len())];
        let short_cp_id = &checkpoint.id.to_string()[..8];
        println!("{} Imported {} as checkpoint {}",
            "✓".green(),
            short_commit.bright_yellow(),
            short_cp_id.bright_cyan()
        );

        // 9. Sync working directory to imported checkpoint
        // Note: delete_extra=false to preserve local files (Git-like behavior)
        println!();
        println!("{}", "Syncing working directory...".dimmed());

        let tree = store.read_tree(checkpoint.root_tree)?;
        let result = restore_tree(&store, &tree, &repo_root, false)?;

        println!("{} Synced {} files", "✓".green(), result.files_restored.to_string().green());
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
        reapply_stash(&repo_root, &store, &journal, &stash_manager, stash).await?;
    }

    println!();
    println!("{}", "Pull complete.".dimmed());

    Ok(())
}

/// Check for uncommitted changes and auto-stash if found
async fn check_and_auto_stash(
    repo_root: &Path,
    store: &Store,
    journal: &Journal,
    stash_manager: &StashManager,
) -> Result<Option<StashEntry>> {
    // Get latest checkpoint
    let latest = match journal.latest()? {
        Some(cp) => cp,
        None => return Ok(None), // No checkpoints yet, nothing to stash
    };

    // Check if working directory differs from latest checkpoint
    let has_changes = check_working_dir_changes(repo_root, store, &latest)?;

    if !has_changes {
        return Ok(None);
    }

    println!("{}", "Auto-stashing uncommitted changes...".dimmed());

    // Force a checkpoint of current state
    // The daemon will create this, but we need to wait for it
    // For now, we'll create a manual checkpoint
    let stash_checkpoint = create_stash_checkpoint(repo_root, store, journal)?;

    // Create stash entry
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis() as u64;

    let entry = StashEntry {
        checkpoint_id: stash_checkpoint.id,
        created_at_ms: now_ms,
        message: Some("Auto-stash before pull".to_string()),
        base_checkpoint_id: latest.parent,
    };

    stash_manager.push(entry.clone())?;

    let short_stash_id = &stash_checkpoint.id.to_string()[..8];
    println!("{} Stashed changes in checkpoint {}",
        "✓".green(),
        short_stash_id.bright_cyan()
    );

    Ok(Some(entry))
}

/// Check if working directory has changes compared to checkpoint
fn check_working_dir_changes(
    repo_root: &Path,
    store: &Store,
    checkpoint: &Checkpoint,
) -> Result<bool> {
    use std::collections::HashSet;

    let tree = store.read_tree(checkpoint.root_tree)?;

    // Get all files in checkpoint
    let checkpoint_files: HashSet<String> = tree.entries_with_paths()
        .filter_map(|(path_bytes, _)| {
            std::str::from_utf8(path_bytes).ok().map(|s| s.to_string())
        })
        .collect();

    // Walk working directory and check for differences
    for entry in walkdir::WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| {
            let path = e.path();
            let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            // Skip hidden directories
            !name.starts_with('.') || path == repo_root
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let rel_path = match path.strip_prefix(repo_root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Skip protected paths
        if rel_path.starts_with(".tl/") || rel_path.starts_with(".git/") || rel_path.starts_with(".jj/") {
            continue;
        }

        // Check if file exists in checkpoint
        if !checkpoint_files.contains(&rel_path) {
            return Ok(true); // New file
        }

        // Check if file content differs
        if let Some(checkpoint_entry) = tree.get(std::path::Path::new(&rel_path)) {
            let current_hash = tl_core::hash::hash_file(path)?;
            if current_hash != checkpoint_entry.blob_hash {
                return Ok(true); // Modified file
            }
        }
    }

    // Check for deleted files
    for file in &checkpoint_files {
        let full_path = repo_root.join(file);
        if !full_path.exists() {
            return Ok(true); // Deleted file
        }
    }

    Ok(false)
}

/// Create a checkpoint of current working directory state (for stash)
///
/// CRITICAL: This now validates stash completeness by tracking walk errors.
/// If any files fail to be captured, the stash is considered incomplete.
fn create_stash_checkpoint(
    repo_root: &Path,
    store: &Store,
    journal: &Journal,
) -> Result<Checkpoint> {
    use tl_core::{Entry, Tree};

    let mut tree = Tree::new();
    let mut files_changed = 0u32;
    let mut walk_errors = Vec::new();

    // Walk working directory
    for entry in walkdir::WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.')
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // Track walk errors instead of silently skipping
                walk_errors.push(format!("Walk error: {}", e));
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let rel_path = match path.strip_prefix(repo_root) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Hash and store blob - track errors
        let blob_hash = match tl_core::hash::hash_file(path) {
            Ok(h) => h,
            Err(e) => {
                walk_errors.push(format!("Hash error {}: {}", path.display(), e));
                continue;
            }
        };

        if !store.blob_store().has_blob(blob_hash) {
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    walk_errors.push(format!("Read error {}: {}", path.display(), e));
                    continue;
                }
            };
            if let Err(e) = store.blob_store().write_blob(blob_hash, &content) {
                walk_errors.push(format!("Write blob error {}: {}", path.display(), e));
                continue;
            }
        }

        // Get file mode
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            match entry.metadata() {
                Ok(m) => m.mode(),
                Err(e) => {
                    walk_errors.push(format!("Metadata error {}: {}", path.display(), e));
                    continue;
                }
            }
        };
        #[cfg(not(unix))]
        let mode = 0o644;

        tree.insert(rel_path, Entry::file(mode, blob_hash));
        files_changed += 1;
    }

    // CRITICAL: Fail if stash is incomplete
    if !walk_errors.is_empty() {
        anyhow::bail!(
            "Stash incomplete - {} file(s) could not be captured:\n  {}",
            walk_errors.len(),
            walk_errors.iter().take(5).cloned().collect::<Vec<_>>().join("\n  ")
        );
    }

    // Store tree
    let tree_hash = tree.hash();
    store.write_tree(&tree)?;

    // Create checkpoint
    let parent = journal.latest()?.map(|cp| cp.id);
    let checkpoint = Checkpoint::new(
        parent,
        tree_hash,
        CheckpointReason::Manual, // Stash is considered manual
        vec![],
        CheckpointMeta {
            files_changed,
            bytes_added: 0,
            bytes_removed: 0,
        },
    );

    journal.append(&checkpoint)?;

    Ok(checkpoint)
}

/// Import a remote JJ commit as a TL checkpoint
fn import_remote_commit(
    workspace: &jj_lib::workspace::Workspace,
    commit_id: &str,
    store: &Store,
    journal: &Journal,
    mapping: &jj::JjMapping,
    no_pin: bool,
    tl_dir: &Path,
) -> Result<Checkpoint> {
    use journal::PinManager;

    // Export JJ commit to temp directory
    let temp_dir = tempfile::tempdir()
        .context("Failed to create temporary directory")?;

    jj::git_ops::export_commit_to_dir(workspace, commit_id, temp_dir.path())?;

    // Import directory as checkpoint
    let (root_tree, files_changed) = import_directory_to_store(temp_dir.path(), store)?;

    // Get parent checkpoint
    let parent = journal.latest()?.map(|cp| cp.id);

    // Create checkpoint
    let checkpoint = Checkpoint::new(
        parent,
        root_tree,
        CheckpointReason::Publish, // Imported from remote
        vec![],
        CheckpointMeta {
            files_changed,
            bytes_added: 0,
            bytes_removed: 0,
        },
    );

    // Write to journal
    journal.append(&checkpoint)?;

    // Store bidirectional mapping
    mapping.set(checkpoint.id, commit_id)?;
    mapping.set_reverse(commit_id, checkpoint.id)?;

    // Auto-pin if requested
    if !no_pin {
        let pin_manager = PinManager::new(tl_dir);
        pin_manager.pin("pulled", checkpoint.id)?;
    }

    Ok(checkpoint)
}

/// Re-apply stashed changes after pull
async fn reapply_stash(
    repo_root: &Path,
    store: &Store,
    journal: &Journal,
    stash_manager: &StashManager,
    stash: StashEntry,
) -> Result<()> {
    println!();
    println!("{}", "Re-applying stashed changes...".dimmed());

    // Get the stash checkpoint
    let stash_checkpoint = journal.get(&stash.checkpoint_id)?
        .ok_or_else(|| anyhow::anyhow!("Stash checkpoint not found"))?;

    // Get current state (after pull)
    let current = journal.latest()?
        .ok_or_else(|| anyhow::anyhow!("No current checkpoint"))?;

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

/// Import a directory into the store, returning root tree hash and file count
fn import_directory_to_store(dir: &Path, store: &Store) -> Result<(tl_core::Sha1Hash, u32)> {
    use std::fs;
    use tl_core::{Entry, Tree};
    use walkdir::WalkDir;

    let mut tree = Tree::new();
    let mut files_changed = 0u32;

    // Walk directory and import all files
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry?;
        let path = entry.path();

        // Skip the directory itself
        if path == dir {
            continue;
        }

        // Get relative path
        let rel_path = path.strip_prefix(dir)?;

        // Skip directories (only process files and symlinks)
        if entry.file_type().is_dir() {
            continue;
        }

        // Handle symlinks
        if entry.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            let target_bytes = target.to_string_lossy();
            let blob_hash = tl_core::hash::hash_bytes(target_bytes.as_bytes());

            if !store.blob_store().has_blob(blob_hash) {
                store.blob_store().write_blob(blob_hash, target_bytes.as_bytes())?;
            }

            tree.insert(rel_path, Entry::symlink(blob_hash));
            files_changed += 1;
            continue;
        }

        // Handle regular files
        if entry.file_type().is_file() {
            let metadata = entry.metadata()?;

            // Extract Unix mode bits
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::MetadataExt;
                metadata.mode()
            };
            #[cfg(not(unix))]
            let mode = if metadata.permissions().readonly() {
                0o444
            } else {
                0o644
            };

            // Hash file
            let blob_hash = if metadata.len() > 4 * 1024 * 1024 {
                tl_core::hash::hash_file_mmap(path)?
            } else {
                tl_core::hash::hash_file(path)?
            };

            // Write blob if not exists
            if !store.blob_store().has_blob(blob_hash) {
                let contents = fs::read(path)?;
                store.blob_store().write_blob(blob_hash, &contents)?;
            }

            tree.insert(rel_path, Entry::file(mode, blob_hash));
            files_changed += 1;
        }
    }

    // Compute tree hash
    let tree_hash = tree.hash();

    // Store tree
    store.write_tree(&tree)?;

    Ok((tree_hash, files_changed))
}
