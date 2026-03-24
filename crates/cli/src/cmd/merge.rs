//! Merge command - 3-way merge from a branch
//!
//! Merges changes from a branch into the current working state.
//!
//! Usage:
//!   tl merge main           # Merge branch into current state
//!   tl merge --abort          # Abort in-progress merge
//!   tl merge --continue       # Complete merge after resolving conflicts

use anyhow::{anyhow, Context, Result};
use crate::util;
use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal};
use jj::{MergeState, write_conflict_markers};
use owo_colors::OwoColorize;
use std::path::Path;
use tl_core::Store;

/// Run the merge command
pub async fn run(
    branch: Option<String>,
    abort: bool,
    continue_merge: bool,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;
    let tl_dir = repo_root.join(".tl");

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // 3. Check for existing merge state
    let merge_state = MergeState::load(&tl_dir)?;

    // Handle --abort
    if abort {
        return handle_abort(&repo_root, &tl_dir, merge_state).await;
    }

    // Handle --continue
    if continue_merge {
        return handle_continue(&repo_root, &tl_dir, merge_state).await;
    }

    // Need a branch to merge
    let branch = branch.ok_or_else(|| anyhow!("Branch name required. Usage: tl merge <branch>"))?;

    // Check for existing merge in progress
    if let Some(state) = merge_state {
        if state.in_progress {
            anyhow::bail!(
                "Merge already in progress.\n\
                 Use 'tl merge --continue' after resolving conflicts, or\n\
                 Use 'tl merge --abort' to cancel the merge."
            );
        }
    }

    // 4. Start the merge
    start_merge(&repo_root, &tl_dir, &branch).await
}

/// Start a new merge operation
async fn start_merge(repo_root: &Path, tl_dir: &Path, branch: &str) -> Result<()> {
    println!("{}", format!("Merging {}...", branch).dimmed());

    // Open components
    let store = Store::open(repo_root)
        .context("Failed to open Timelapse store")?;
    let journal = Journal::open(tl_dir)
        .context("Failed to open journal")?;

    // Get current checkpoint for abort recovery
    let current_checkpoint = journal.latest()?
        .ok_or_else(|| anyhow!("No checkpoints found. Create one first."))?;

    let pre_merge_checkpoint_id = current_checkpoint.id.to_string();

    // Load JJ workspace
    let workspace = jj::load_workspace(repo_root)
        .context("Failed to load JJ workspace")?;

    // Perform the merge
    let merge_result = jj::perform_merge(&workspace, branch)
        .context("Failed to perform merge")?;

    // Check if merge was clean
    if merge_result.is_clean {
        println!("{} Merge completed cleanly (no conflicts)", "✓".green());

        // For clean merge, we could create a merge checkpoint here
        // For now, the daemon will pick up the working directory changes
        println!();
        println!("{}", "Working directory updated. The daemon will create a checkpoint.".dimmed());

        return Ok(());
    }

    // Merge has conflicts - write conflict markers to files
    println!();
    println!("{} Merge has {} conflicts:", "!".yellow(), merge_result.conflicts.len().to_string().red());
    println!();

    let mut conflict_paths = Vec::new();

    for conflict in &merge_result.conflicts {
        let file_path = repo_root.join(&conflict.path);
        println!("  {} {}", "✗".red(), conflict.path);

        // Write conflict markers to file
        write_conflict_markers(
            &file_path,
            conflict.base_content.as_deref(),
            &conflict.ours_content,
            &conflict.theirs_content,
            "LOCAL (your changes)",
            &format!("REMOTE ({})", branch),
        ).context(format!("Failed to write conflict markers to {}", conflict.path))?;

        conflict_paths.push(conflict.path.clone());
    }

    // Save merge state
    let state = MergeState {
        in_progress: true,
        ours_commit: merge_result.ours_commit_id,
        theirs_commit: merge_result.theirs_commit_id,
        theirs_branch: branch.to_string(),
        base_commit: merge_result.base_commit_id,
        conflicts: conflict_paths,
        pre_merge_checkpoint: pre_merge_checkpoint_id,
    };

    state.save(tl_dir)?;

    println!();
    println!("{}", "To resolve:".bold());
    println!("  1. Edit the conflicted files to resolve the conflicts");
    println!("  2. Run 'tl resolve --list' to check resolution status");
    println!("  3. Run 'tl merge --continue' to complete the merge");
    println!();
    println!("{}", "To abort:".dimmed());
    println!("  Run 'tl merge --abort' to restore pre-merge state");

    Ok(())
}

/// Abort an in-progress merge
async fn handle_abort(repo_root: &Path, tl_dir: &Path, merge_state: Option<MergeState>) -> Result<()> {
    let state = match merge_state {
        Some(s) if s.in_progress => s,
        _ => anyhow::bail!("No merge in progress to abort."),
    };

    println!("{}", "Aborting merge...".dimmed());

    // Restore pre-merge checkpoint
    let store = Store::open(repo_root)
        .context("Failed to open store")?;
    let journal = Journal::open(tl_dir)
        .context("Failed to open journal")?;

    // Parse checkpoint ID
    let checkpoint_id: ulid::Ulid = state.pre_merge_checkpoint.parse()
        .context("Invalid pre-merge checkpoint ID")?;

    // Get the checkpoint
    let checkpoint = journal.get(&checkpoint_id)?
        .ok_or_else(|| anyhow!("Pre-merge checkpoint not found"))?;

    // Restore tree
    let tree = store.read_tree(checkpoint.root_tree)?;
    let result = crate::cmd::restore::restore_tree(&store, &tree, repo_root, true)?;

    // Clear merge state
    MergeState::clear(tl_dir)?;

    println!("{} Merge aborted, restored {} files", "✓".green(), result.files_restored);

    Ok(())
}

/// Continue a merge after conflicts are resolved
async fn handle_continue(repo_root: &Path, tl_dir: &Path, merge_state: Option<MergeState>) -> Result<()> {
    let state = match merge_state {
        Some(s) if s.in_progress => s,
        _ => anyhow::bail!("No merge in progress."),
    };

    println!("{}", "Checking conflict resolution...".dimmed());

    // Check all conflicts are resolved
    let mut unresolved = Vec::new();

    for path in &state.conflicts {
        let file_path = repo_root.join(path);
        if jj::has_conflict_markers(&file_path)? {
            unresolved.push(path.clone());
        }
    }

    if !unresolved.is_empty() {
        println!("{} {} file(s) still have conflicts:", "✗".red(), unresolved.len());
        for path in &unresolved {
            println!("  {} {}", "✗".red(), path);
        }
        println!();
        println!("Resolve the conflicts and run 'tl merge --continue' again.");
        anyhow::bail!("Unresolved conflicts remain");
    }

    println!("{} All conflicts resolved", "✓".green());

    // Create a checkpoint with the merged state
    let store = Store::open(repo_root)
        .context("Failed to open store")?;
    let journal = Journal::open(tl_dir)
        .context("Failed to open journal")?;

    // Create checkpoint from current working directory
    let checkpoint = create_merge_checkpoint(repo_root, &store, &journal, &state)?;

    let short_id = &checkpoint.id.to_string()[..8];
    println!("{} Created merge checkpoint {}", "✓".green(), short_id.bright_cyan());

    // Clear merge state
    MergeState::clear(tl_dir)?;

    println!();
    println!("{}", "Merge complete.".green().bold());

    Ok(())
}

/// Create a checkpoint representing the merged state
fn create_merge_checkpoint(
    repo_root: &Path,
    store: &Store,
    journal: &Journal,
    merge_state: &MergeState,
) -> Result<Checkpoint> {
    use tl_core::{Entry, Tree};

    let mut tree = Tree::new();
    let mut files_changed = 0u32;

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
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let rel_path = match path.strip_prefix(repo_root) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Hash and store blob
        let blob_hash = tl_core::hash::hash_file(path)?;

        if !store.blob_store().has_blob(blob_hash) {
            let content = std::fs::read(path)?;
            store.blob_store().write_blob(blob_hash, &content)?;
        }

        // Get file mode
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            entry.metadata()?.mode()
        };
        #[cfg(not(unix))]
        let mode = 0o644;

        tree.insert(rel_path, Entry::file(mode, blob_hash));
        files_changed += 1;
    }

    // Store tree
    let tree_hash = tree.hash();
    store.write_tree(&tree)?;

    // Create checkpoint with merge info in description
    let parent = journal.latest()?.map(|cp| cp.id);
    let checkpoint = Checkpoint::new(
        parent,
        tree_hash,
        CheckpointReason::Manual, // Merge checkpoint
        vec![], // touched_paths - empty for merge checkpoints
        CheckpointMeta {
            files_changed,
            bytes_added: 0,
            bytes_removed: 0,
        },
    );

    journal.append(&checkpoint)?;

    Ok(checkpoint)
}
