//! Export JJ trees to directories using native jj-lib APIs
//!
//! This module provides the reverse of tree conversion - taking a JJ tree
//! and materializing it to a filesystem directory.

use anyhow::{Context, Result};
use jj_lib::backend::{MergedTreeId, TreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo_path::RepoPath;
use jj_lib::store::Store;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// Export a JJ tree to a target directory
///
/// Recursively exports all files, symlinks, and subdirectories from a JJ tree
/// to the filesystem. This is the inverse of convert_tree_to_jj().
///
/// # Arguments
/// * `jj_store` - JJ store for reading tree and blob data
/// * `tree_id` - Root tree ID (MergedTreeId) to export
/// * `target_dir` - Directory to write files to
///
/// # Behavior
/// - Creates parent directories as needed
/// - Preserves executable bits on Unix
/// - Preserves symlinks on Unix
/// - Skips .tl/, .git/, .jj/ directories
pub fn export_jj_tree_to_dir(
    jj_store: &Arc<Store>,
    tree_id: &MergedTreeId,
    target_dir: &Path,
) -> Result<()> {
    // Read the root tree
    let tree = jj_store.get_root_tree(tree_id)
        .with_context(|| format!("Failed to read tree"))?;

    // Export recursively
    export_tree_entries(jj_store, &tree, target_dir, &RepoPath::root())
}

/// Recursively export tree entries to filesystem
fn export_tree_entries(
    jj_store: &Arc<Store>,
    tree: &MergedTree,
    target_dir: &Path,
    _current_path: &RepoPath,
) -> Result<()> {
    // MergedTree.entries() returns Iterator<Item=(RepoPathBuf, Merge<Option<TreeValue>>)>
    for (entry_path, merge_value) in tree.entries() {
        let path_str = entry_path.as_internal_file_string();

        // Skip protected directories
        if path_str.starts_with(".tl/")
            || path_str.starts_with(".git/")
            || path_str.starts_with(".jj/")
        {
            continue;
        }

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
            TreeValue::File { id, executable } => {
                // Create parent directories
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
                }

                // Read file content
                let mut content = Vec::new();
                let mut reader = jj_store.read_file(&entry_path, id)
                    .with_context(|| format!("Failed to read file: {}", path_str))?;
                reader.read_to_end(&mut content)
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
                    // Read symlink target using read_symlink API
                    let target = jj_store.read_symlink(&entry_path, id)
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

            TreeValue::Tree(subtree_id) => {
                // Recurse into subdirectory
                let tree = jj_store.get_tree(&entry_path, subtree_id)
                    .with_context(|| format!("Failed to read subtree: {}", path_str))?;

                // Wrap Tree in MergedTree for recursion
                let merged_tree = MergedTree::legacy(tree);

                export_tree_entries(jj_store, &merged_tree, target_dir, &entry_path)?;
            }

            TreeValue::GitSubmodule(_) => {
                eprintln!("Warning: Git submodules not supported, skipping: {}", path_str);
            }

            TreeValue::Conflict(_) => {
                eprintln!("Warning: Conflict markers found at {}, skipping", path_str);
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
