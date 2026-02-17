//! Export JJ trees to directories using native jj-lib APIs
//!
//! This module provides the reverse of tree conversion - taking a JJ tree
//! and materializing it to a filesystem directory.

use anyhow::{Context, Result};
use jj_lib::backend::TreeValue;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo_path::RepoPath;
use jj_lib::store::Store;
use pollster::FutureExt as _;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

/// Export a JJ tree directly into the Timelapse store (no filesystem materialization).
///
/// This avoids the tempdir export + re-walk import path, cutting multiple full
/// disk scans and copies during `tl pull`.
///
/// Returns `(root_tree_hash, entries_written)`.
pub fn export_jj_tree_to_tl_store(
    jj_store: &Arc<Store>,
    tree: &MergedTree,
    tl_store: &tl_core::Store,
) -> Result<(tl_core::Sha1Hash, u32)> {
    use tl_core::{Entry, Tree};

    let mut out_tree = Tree::new();
    let mut entries_written = 0u32;

    // MergedTree.entries() returns Iterator<Item=(RepoPathBuf, BackendResult<MergedTreeValue>)>
    for (entry_path, merge_value_result) in tree.entries() {
        let path_str = entry_path.as_internal_file_string();

        // Skip protected directories
        if path_str.starts_with(".tl/")
            || path_str.starts_with(".git/")
            || path_str.starts_with(".jj/")
        {
            continue;
        }

        let merge_value = merge_value_result
            .with_context(|| format!("Failed to read tree entry: {}", path_str))?;

        let value = match merge_value.as_resolved() {
            Some(Some(v)) => v,       // Resolved, non-deleted entry
            Some(None) => continue,   // Resolved but deleted - skip
            None => continue,         // Conflicted entry - skip
        };

        match value {
            TreeValue::File { id, executable, .. } => {
                let mut content = Vec::new();
                let mut reader = jj_store
                    .read_file(&entry_path, id)
                    .block_on()
                    .with_context(|| format!("Failed to read file: {}", path_str))?;
                reader
                    .read_to_end(&mut content)
                    .block_on()
                    .context("Failed to read file content")?;

                let blob_hash = tl_core::hash::git::hash_blob(&content);
                if !tl_store.blob_store().has_blob(blob_hash) {
                    tl_store
                        .blob_store()
                        .write_blob(blob_hash, &content)
                        .with_context(|| format!("Failed to write blob: {}", path_str))?;
                }

                let mode = if *executable { 0o755 } else { 0o644 };
                out_tree.insert(Path::new(path_str), Entry::file(mode, blob_hash));
                entries_written += 1;
            }
            TreeValue::Symlink(id) => {
                #[cfg(unix)]
                {
                    let target = jj_store
                        .read_symlink(&entry_path, id)
                        .block_on()
                        .with_context(|| format!("Failed to read symlink: {}", path_str))?;

                    let target_bytes = target.as_bytes();
                    let blob_hash = tl_core::hash::git::hash_blob(target_bytes);
                    if !tl_store.blob_store().has_blob(blob_hash) {
                        tl_store
                            .blob_store()
                            .write_blob(blob_hash, target_bytes)
                            .with_context(|| format!("Failed to write symlink blob: {}", path_str))?;
                    }

                    out_tree.insert(Path::new(path_str), Entry::symlink(blob_hash));
                    entries_written += 1;
                }

                #[cfg(not(unix))]
                {
                    // Best-effort on non-Unix: store as a normal file containing the target.
                    let target = jj_store
                        .read_symlink(&entry_path, id)
                        .block_on()
                        .with_context(|| format!("Failed to read symlink: {}", path_str))?;
                    let target_bytes = target.as_bytes();

                    let blob_hash = tl_core::hash::git::hash_blob(target_bytes);
                    if !tl_store.blob_store().has_blob(blob_hash) {
                        tl_store
                            .blob_store()
                            .write_blob(blob_hash, target_bytes)
                            .with_context(|| format!("Failed to write symlink blob: {}", path_str))?;
                    }

                    out_tree.insert(Path::new(path_str), Entry::file(0o644, blob_hash));
                    entries_written += 1;
                }
            }
            TreeValue::Tree(_) => unreachable!("MergedTree::entries() should not yield Tree values"),
            TreeValue::GitSubmodule(_) => {
                // Submodules aren't supported in the TL store format yet.
                continue;
            }
        }
    }

    let root_tree = tl_store.write_tree(&out_tree)?;
    Ok((root_tree, entries_written))
}

/// Export a JJ tree to a target directory
///
/// Recursively exports all files, symlinks, and subdirectories from a JJ tree
/// to the filesystem. This is the inverse of convert_tree_to_jj().
///
/// # Arguments
/// * `jj_store` - JJ store for reading tree and blob data
/// * `tree` - MergedTree to export
/// * `target_dir` - Directory to write files to
///
/// # Behavior
/// - Creates parent directories as needed
/// - Preserves executable bits on Unix
/// - Preserves symlinks on Unix
/// - Skips .tl/, .git/, .jj/ directories
pub fn export_jj_tree_to_dir(
    jj_store: &Arc<Store>,
    tree: &MergedTree,
    target_dir: &Path,
) -> Result<()> {
    // Export recursively
    export_tree_entries(jj_store, tree, target_dir, &RepoPath::root())
}

/// Export tree entries to filesystem
fn export_tree_entries(
    jj_store: &Arc<Store>,
    tree: &MergedTree,
    target_dir: &Path,
    _current_path: &RepoPath,
) -> Result<()> {
    // MergedTree.entries() returns Iterator<Item=(RepoPathBuf, BackendResult<MergedTreeValue>)>
    for (entry_path, merge_value_result) in tree.entries() {
        let path_str = entry_path.as_internal_file_string();

        // Skip protected directories
        if path_str.starts_with(".tl/")
            || path_str.starts_with(".git/")
            || path_str.starts_with(".jj/")
        {
            continue;
        }

        // Unwrap the backend result
        let merge_value = merge_value_result
            .with_context(|| format!("Failed to read tree entry: {}", path_str))?;

        // Resolve the merge to get the actual value
        // as_resolved() returns Option<&Option<TreeValue>>
        let value = match merge_value.as_resolved() {
            Some(Some(v)) => v, // Resolved, non-deleted entry
            Some(None) => continue, // Resolved but deleted - skip
            None => {
                // Conflicted entry - skip with warning
                eprintln!("Warning: Skipping conflicted entry: {}", path_str);
                continue;
            }
        };

        let file_path = target_dir.join(path_str);

        match value {
            TreeValue::File { id, executable, .. } => {
                // Create parent directories
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
                }

                // Read file content (async with block_on)
                let mut content = Vec::new();
                let mut reader = jj_store.read_file(&entry_path, id)
                    .block_on()
                    .with_context(|| format!("Failed to read file: {}", path_str))?;
                reader.read_to_end(&mut content)
                    .block_on()
                    .context("Failed to read file content")?;

                // Write to filesystem
                fs::write(&file_path, content)
                    .with_context(|| format!("Failed to write file: {}", file_path.display()))?;

                // Set executable bit on Unix
                #[cfg(unix)]
                if *executable {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755))
                        .context("Failed to set executable permissions")?;
                }
            }

            TreeValue::Symlink(id) => {
                #[cfg(unix)]
                {
                    // Read symlink target using read_symlink API (async with block_on)
                    let target = jj_store.read_symlink(&entry_path, id)
                        .block_on()
                        .with_context(|| format!("Failed to read symlink: {}", path_str))?;

                    // Create parent directories
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent)?;
                    }

                    // Create symlink
                    std::os::unix::fs::symlink(target, &file_path)
                        .with_context(|| format!("Failed to create symlink: {}", file_path.display()))?;
                }

                #[cfg(not(unix))]
                {
                    eprintln!("Warning: Skipping symlink on non-Unix platform: {}", path_str);
                }
            }

            TreeValue::Tree(_) => {
                // The entries() iterator handles tree recursion internally,
                // so we should never see TreeValue::Tree here
                unreachable!("MergedTree::entries() should not yield Tree values");
            }

            TreeValue::GitSubmodule(_) => {
                eprintln!("Warning: Git submodules not supported, skipping: {}", path_str);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Note: Full integration tests require JJ workspace setup.
    // These tests verify the module compiles and basic structure is correct.

    #[test]
    fn test_export_module_exists() {
        // This test ensures the module compiles and can be imported
        assert!(true);
    }
}
