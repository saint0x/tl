//! Restore working tree to a checkpoint
//!
//! This module provides both the CLI command and reusable restore utilities
//! for use by other commands (e.g., pull, stash).

use crate::locks::RestoreLock;
use crate::util;
use anyhow::{anyhow, Context, Result};
use tl_core::{Entry, Store, Tree};
use owo_colors::OwoColorize;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub async fn run(checkpoint: &str, skip_confirm: bool) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Acquire restore lock to prevent daemon race conditions
    // CRITICAL: This prevents the daemon from creating checkpoints during restore,
    // which could corrupt the pathmap or capture partial state
    let _restore_lock = RestoreLock::acquire(&tl_dir)
        .context("Failed to acquire restore lock - is another restore in progress?")?;

    // 3. Ensure daemon running (auto-starts if needed)
    crate::daemon::ensure_daemon_running().await?;

    // 3. Resolve checkpoint reference via unified data access layer
    let ids = crate::data_access::resolve_checkpoint_refs(&[checkpoint.to_string()], &tl_dir).await?;
    let checkpoint_id = ids[0].ok_or_else(||
        anyhow!("Checkpoint '{}' not found or ambiguous", checkpoint))?;

    // 4. Get checkpoint via unified data access layer
    let checkpoints = crate::data_access::get_checkpoints(&[checkpoint_id], &tl_dir).await?;
    let cp = checkpoints[0].as_ref().ok_or_else(|| anyhow!("Checkpoint not found"))?;

    // 5. Open store and load target tree
    let store = Store::open(&repo_root)?;
    let tree = store.read_tree(cp.root_tree)?;

    // 6. Confirm with user
    println!("{}", "Restore Checkpoint".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    let id_short = checkpoint_id.to_string()[..8].to_string();
    println!("Checkpoint: {} {}", id_short.yellow(), util::format_relative_time(cp.ts_unix_ms).dimmed());
    println!("Files:      {}", tree.len());
    println!();

    // Conditional confirmation
    if !skip_confirm {
        println!("{}", "⚠️  Warning: This will overwrite your current working directory!".red().bold());
        println!();

        print!("Continue? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("{}", "Restore cancelled".yellow());
            return Ok(());
        }
    } else {
        println!("{}", "⚠️  Restoring without confirmation (--yes flag)".yellow());
    }

    println!();
    println!("{}", "Restoring files...".dimmed());

    // 7. Restore files
    let mut restored = 0;
    let mut errors = Vec::new();

    for (path_bytes, entry) in tree.entries_with_paths() {
        let path_str = std::str::from_utf8(path_bytes)
            .context("Invalid UTF-8 in file path")?;
        let file_path = repo_root.join(path_str);

        // Skip if path tries to escape repository or touch protected directories
        if !file_path.starts_with(&repo_root) {
            errors.push(format!("Path escapes repository: {}", path_str));
            continue;
        }

        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") {
            continue; // Skip protected directories
        }

        // Restore file
        match restore_file(&store, &file_path, entry) {
            Ok(()) => {
                restored += 1;
                if restored % 100 == 0 {
                    print!("  Restored {} files...\r", restored);
                    std::io::stdout().flush().ok();
                }
            }
            Err(e) => {
                errors.push(format!("{}: {}", path_str, e));
            }
        }
    }

    println!();
    println!();

    // 8. Display results
    if errors.is_empty() {
        println!("{} Restored {} files successfully", "✓".green(), restored.to_string().green());
    } else {
        println!("{} Restored {} files with {} errors", "⚠".yellow(), restored, errors.len());
        println!();
        println!("{}", "Errors:".red().bold());
        for error in errors.iter().take(10) {
            println!("  {}", error.red());
        }
        if errors.len() > 10 {
            println!("  ... and {} more", errors.len() - 10);
        }
    }

    println!();
    println!("{}", "Note: The daemon will automatically create a new checkpoint reflecting these changes.".dimmed());

    Ok(())
}

// =============================================================================
// Reusable restore utilities (used by restore, pull, stash)
// =============================================================================

/// Restore a single file from a checkpoint entry
///
/// This is a low-level utility used by restore_tree and the restore command.
pub fn restore_file(store: &Store, file_path: &Path, entry: &Entry) -> Result<()> {
    // Create parent directories
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read blob content
    let content = store.blob_store().read_blob(entry.blob_hash)?;

    // Write file
    fs::write(file_path, content)?;

    // Set permissions
    #[cfg(unix)]
    {
        let mode = entry.mode;
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(file_path, permissions)?;
    }

    Ok(())
}

/// Result of a tree restore operation
#[derive(Debug, Default)]
pub struct RestoreResult {
    pub files_restored: usize,
    pub files_deleted: usize,
    pub errors: Vec<String>,
}

/// Restore an entire tree to the working directory
///
/// This is the core restore logic, reusable by pull, stash pop, etc.
///
/// # Arguments
/// * `store` - The blob/tree store
/// * `tree` - The tree to restore
/// * `repo_root` - Repository root path
/// * `delete_extra` - If true, delete files not in the tree
///
/// # Returns
/// RestoreResult with counts and any errors
pub fn restore_tree(
    store: &Store,
    tree: &Tree,
    repo_root: &Path,
    delete_extra: bool,
) -> Result<RestoreResult> {
    let mut result = RestoreResult::default();

    // Track files we're restoring (for delete_extra logic)
    let mut restored_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // Restore all files from tree
    for (path_bytes, entry) in tree.entries_with_paths() {
        let path_str = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(e) => {
                result.errors.push(format!("Invalid UTF-8 in path: {}", e));
                continue;
            }
        };

        let file_path = repo_root.join(path_str);

        // Skip if path escapes repository or touches protected directories
        if !file_path.starts_with(repo_root) {
            result.errors.push(format!("Path escapes repository: {}", path_str));
            continue;
        }

        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") || path_str.starts_with(".jj/") {
            continue;
        }

        // Restore the file
        match restore_file(store, &file_path, entry) {
            Ok(()) => {
                result.files_restored += 1;
                restored_paths.insert(file_path);
            }
            Err(e) => {
                result.errors.push(format!("{}: {}", path_str, e));
            }
        }
    }

    // Optionally delete extra files not in the tree
    if delete_extra {
        let delete_result = delete_extra_files(repo_root, &restored_paths);
        result.files_deleted = delete_result.deleted;

        // Report deletion errors
        for (path, err) in delete_result.errors {
            result.errors.push(format!(
                "Failed to delete {}: {}",
                path.display(),
                err
            ));
        }
    }

    Ok(result)
}

/// Result of delete_extra_files operation
#[derive(Debug, Default)]
pub struct DeleteResult {
    pub deleted: usize,
    pub errors: Vec<(PathBuf, std::io::Error)>,
}

/// Delete files in working directory that aren't in the restored set
///
/// CRITICAL: This function now reports all errors instead of silently ignoring them.
/// Previously, deletion failures were silently swallowed with `.is_ok()`.
fn delete_extra_files(repo_root: &Path, keep_paths: &std::collections::HashSet<PathBuf>) -> DeleteResult {
    let mut result = DeleteResult::default();

    for entry in walkdir::WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| {
            let path = e.path();
            // Skip protected directories entirely
            !path.ends_with(".tl") &&
            !path.ends_with(".git") &&
            !path.ends_with(".jj") &&
            !path.starts_with(repo_root.join(".tl")) &&
            !path.starts_with(repo_root.join(".git")) &&
            !path.starts_with(repo_root.join(".jj"))
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.file_type().is_file() {
            let path = entry.path().to_path_buf();
            if !keep_paths.contains(&path) {
                match fs::remove_file(&path) {
                    Ok(()) => result.deleted += 1,
                    Err(e) => result.errors.push((path, e)),
                }
            }
        }
    }

    result
}
