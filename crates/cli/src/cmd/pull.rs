//! Pull from Git remote and optionally import to checkpoints

use anyhow::{Context, Result};
use crate::util;
use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal, PinManager};
use owo_colors::OwoColorize;
use std::path::Path;
use std::process::Command;
use tl_core::Store;

pub async fn run(
    fetch_only: bool,
    no_pin: bool,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Run jj git fetch
    println!("{}", "Fetching from Git remote...".dimmed());
    let output = Command::new("jj")
        .current_dir(&repo_root)
        .args(&["git", "fetch"])
        .output()
        .context("Failed to execute jj fetch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("JJ fetch failed: {}", stderr);
    }

    println!("{} Fetched from remote", "✓".green());

    if fetch_only {
        println!();
        println!("{}", "Fetch complete. Use 'jj rebase' to integrate changes.".dimmed());
        return Ok(());
    }

    // 4. Import JJ HEAD as checkpoint
    println!();
    println!("{}", "Importing JJ HEAD as checkpoint...".dimmed());

    let tl_dir = repo_root.join(".tl");
    let store = Store::open(&repo_root)
        .context("Failed to open Timelapse store")?;
    let journal = Journal::open(&tl_dir)
        .context("Failed to open journal")?;
    let mapping = jj::JjMapping::open(&tl_dir)
        .context("Failed to open JJ mapping")?;

    match import_jj_head(&repo_root, &tl_dir, &store, &journal, &mapping, no_pin)? {
        ImportResult::AlreadyImported(checkpoint_id) => {
            println!("{} JJ HEAD already imported as checkpoint {}", "✓".green(), checkpoint_id.bright_cyan());
        }
        ImportResult::NewCheckpoint(checkpoint_id, jj_commit_id) => {
            let short_commit = if jj_commit_id.len() >= 12 {
                &jj_commit_id[..12]
            } else {
                &jj_commit_id
            };
            println!("{} Imported JJ commit {} as checkpoint {}",
                "✓".green(),
                short_commit.bright_yellow(),
                checkpoint_id.bright_cyan()
            );

            if !no_pin {
                println!("{} Auto-pinned as {}", "✓".green(), "imported".bright_magenta());
            }
        }
        ImportResult::EmptyRepository => {
            println!("{}", "Repository is empty, nothing to import".dimmed());
        }
    }

    Ok(())
}

/// Result of importing JJ HEAD
enum ImportResult {
    /// Checkpoint already exists for this JJ commit
    AlreadyImported(ulid::Ulid),
    /// Created new checkpoint
    NewCheckpoint(ulid::Ulid, String),
    /// Repository is empty
    EmptyRepository,
}

/// Import JJ HEAD commit as a checkpoint
fn import_jj_head(
    repo_root: &Path,
    tl_dir: &Path,
    store: &Store,
    journal: &Journal,
    mapping: &jj::JjMapping,
    no_pin: bool,
) -> Result<ImportResult> {
    // 1. Get JJ HEAD commit ID
    let jj_commit_id = get_jj_head_commit_id(repo_root)?;

    // Handle empty repository
    if jj_commit_id.is_empty() || jj_commit_id == "0000000000000000000000000000000000000000" {
        return Ok(ImportResult::EmptyRepository);
    }

    // 2. Check if already mapped
    if let Some(checkpoint_id) = mapping.get_checkpoint(&jj_commit_id)? {
        return Ok(ImportResult::AlreadyImported(checkpoint_id));
    }

    // 3. Export JJ commit to temp directory
    let temp_dir = tempfile::tempdir_in(repo_root)
        .context("Failed to create temporary directory")?;

    export_jj_commit_to_dir(repo_root, temp_dir.path())?;

    // 4. Import directory as checkpoint
    let (root_tree, files_changed) = import_directory_to_store(temp_dir.path(), store)?;

    // 5. Get parent checkpoint (current HEAD)
    let parent = journal.latest()?.map(|cp| cp.id);

    // 6. Create checkpoint
    let checkpoint = Checkpoint::new(
        parent,
        root_tree,
        CheckpointReason::Publish, // JJ imports are considered "publish" operations
        vec![], // We don't track individual touched paths for full imports
        CheckpointMeta {
            files_changed,
            bytes_added: 0, // Not tracked for imports
            bytes_removed: 0,
        },
    );

    // 7. Write checkpoint to journal
    journal.append(&checkpoint)?;

    // 8. Store bidirectional mapping
    mapping.set(checkpoint.id, &jj_commit_id)?;
    mapping.set_reverse(&jj_commit_id, checkpoint.id)?;

    // 9. Auto-pin if requested
    if !no_pin {
        let pin_manager = PinManager::new(tl_dir);
        pin_manager.pin("imported", checkpoint.id)?;
    }

    Ok(ImportResult::NewCheckpoint(checkpoint.id, jj_commit_id))
}

/// Get JJ HEAD commit ID
fn get_jj_head_commit_id(repo_root: &Path) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(repo_root)
        .args(&["log", "-r", "@", "-T", "commit_id", "--no-graph"])
        .output()
        .context("Failed to get JJ HEAD commit ID")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to get JJ HEAD: {}", stderr);
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Export JJ commit files to a directory
fn export_jj_commit_to_dir(repo_root: &Path, target_dir: &Path) -> Result<()> {
    use std::fs;

    // Get list of files in JJ commit
    let output = Command::new("jj")
        .current_dir(repo_root)
        .args(&["file", "list"])
        .output()
        .context("Failed to list JJ files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to list JJ files: {}", stderr);
    }

    let files_list = String::from_utf8(output.stdout)?;

    // Export each file
    for file_path_str in files_list.lines() {
        let file_path_str = file_path_str.trim();
        if file_path_str.is_empty() {
            continue;
        }

        // Skip protected directories
        if file_path_str.starts_with(".tl/")
            || file_path_str.starts_with(".git/")
            || file_path_str.starts_with(".jj/") {
            continue;
        }

        // Get file content from JJ
        let output = Command::new("jj")
            .current_dir(repo_root)
            .args(&["file", "show", file_path_str])
            .output()
            .with_context(|| format!("Failed to export file: {}", file_path_str))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Warning: Failed to export {}: {}", file_path_str, stderr);
            continue;
        }

        // Write to temp directory
        let target_file = target_dir.join(file_path_str);

        // Create parent directories
        if let Some(parent) = target_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        fs::write(&target_file, output.stdout)
            .with_context(|| format!("Failed to write file: {}", target_file.display()))?;
    }

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
