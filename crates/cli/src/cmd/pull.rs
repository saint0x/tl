//! Pull from Git remote and optionally import to checkpoints (100% Native - No CLI)

use anyhow::{Context, Result};
use crate::util;
use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal, PinManager};
use owo_colors::OwoColorize;
use std::path::Path;
use tl_core::Store;

pub async fn run(
    fetch_only: bool,
    no_pin: bool,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Fetch using native git API
    println!("{}", "Fetching from Git remote...".dimmed());
    let mut workspace = jj::load_workspace(&repo_root)
        .context("Failed to load JJ workspace")?;

    jj::git_ops::native_git_fetch(&mut workspace)?;

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

    match import_jj_head(&workspace, &tl_dir, &store, &journal, &mapping, no_pin)? {
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
    workspace: &jj_lib::workspace::Workspace,
    tl_dir: &Path,
    store: &Store,
    journal: &Journal,
    mapping: &jj::JjMapping,
    no_pin: bool,
) -> Result<ImportResult> {
    // 1. Get JJ HEAD commit ID using native API
    let jj_commit_id = get_jj_head_commit_id_native(workspace)?;

    // Handle empty repository
    if jj_commit_id.is_empty() || jj_commit_id == "0000000000000000000000000000000000000000" {
        return Ok(ImportResult::EmptyRepository);
    }

    // 2. Check if already mapped
    if let Some(checkpoint_id) = mapping.get_checkpoint(&jj_commit_id)? {
        return Ok(ImportResult::AlreadyImported(checkpoint_id));
    }

    // 3. Export JJ commit to temp directory using native API
    let temp_dir = tempfile::tempdir()
        .context("Failed to create temporary directory")?;

    export_jj_commit_to_dir_native(workspace, temp_dir.path())?;

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

/// Get JJ HEAD commit ID using native API
fn get_jj_head_commit_id_native(workspace: &jj_lib::workspace::Workspace) -> Result<String> {
    use jj_lib::backend::ObjectId;
    use jj_lib::repo::Repo;

    let config = config::Config::builder().build()?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repository")?;

    let wc_commit_id = repo.view()
        .get_wc_commit_id(workspace.workspace_id())
        .ok_or_else(|| anyhow::anyhow!("No working copy commit found"))?;

    Ok(wc_commit_id.hex())
}

/// Export JJ commit files to a directory using native API
fn export_jj_commit_to_dir_native(
    workspace: &jj_lib::workspace::Workspace,
    target_dir: &Path,
) -> Result<()> {
    use jj_lib::repo::Repo;

    let config = config::Config::builder().build()?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repository")?;

    let wc_commit_id = repo.view()
        .get_wc_commit_id(workspace.workspace_id())
        .ok_or_else(|| anyhow::anyhow!("No working copy commit found"))?;

    let commit = repo.store().get_commit(wc_commit_id)
        .context("Failed to get working copy commit")?;

    let tree_id = commit.tree_id();

    // REUSE existing export.rs function - already tested!
    jj::export::export_jj_tree_to_dir(
        repo.store(),
        tree_id,
        target_dir,
    )?;

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
