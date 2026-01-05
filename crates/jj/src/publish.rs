//! Publish checkpoints to JJ commits
//!
//! This module handles converting Timelapse checkpoints into JJ commits.
//! It uses a hybrid approach:
//! - Materializes checkpoint trees to temp directories (using Timelapse APIs)
//! - Creates JJ commits via CLI (battle-tested, handles all edge cases)
//!
//! This is the production-ready approach used by real jj integrations.

use anyhow::{Context, Result};
use journal::Checkpoint;
use std::fs;
use std::path::Path;
use tl_core::Store;

use crate::mapping::JjMapping;
use crate::materialize::PublishOptions;

/// Materialize a checkpoint tree to a target directory
///
/// This recreates the exact file structure from the checkpoint in the given directory.
pub fn materialize_checkpoint_to_dir(
    checkpoint: &Checkpoint,
    store: &Store,
    target_dir: &Path,
) -> Result<()> {
    // Load the tree
    let tree = store.read_tree(checkpoint.root_tree)
        .context("Failed to read checkpoint tree")?;

    // Restore each file (pattern from restore.rs)
    for (path_bytes, entry) in tree.entries_with_paths() {
        let path_str = std::str::from_utf8(path_bytes)
            .context("Invalid UTF-8 in file path")?;

        // Skip protected directories
        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") || path_str.starts_with(".jj/") {
            continue;
        }

        let file_path = target_dir.join(path_str);

        // Create parent directories
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        // Read blob content
        let content = store.blob_store().read_blob(entry.blob_hash)
            .with_context(|| format!("Failed to read blob for {}", path_str))?;

        // Handle symlinks
        if entry.kind == tl_core::EntryKind::Symlink {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                let target = std::str::from_utf8(&content)
                    .with_context(|| format!("Invalid UTF-8 in symlink target: {}", path_str))?;
                symlink(target, &file_path)
                    .with_context(|| format!("Failed to create symlink: {}", file_path.display()))?;
                continue; // Skip regular file handling
            }
            #[cfg(not(unix))]
            {
                eprintln!("Warning: Symlinks not supported on Windows, writing as file: {}", path_str);
                // Fall through to write as regular file
            }
        }

        // Write file
        fs::write(&file_path, content)
            .with_context(|| format!("Failed to write file: {}", file_path.display()))?;

        // Set permissions (Unix)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(entry.mode);
            fs::set_permissions(&file_path, permissions)
                .with_context(|| format!("Failed to set permissions: {}", file_path.display()))?;
        }
    }

    Ok(())
}

/// Copy directory recursively
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Publish a single checkpoint to JJ
///
/// This creates a JJ commit from the checkpoint using native jj-lib APIs.
/// Delegates to the native implementation in materialize.rs.
pub fn publish_checkpoint(
    checkpoint: &Checkpoint,
    store: &Store,
    repo_root: &Path,
    mapping: &JjMapping,
    options: &PublishOptions,
) -> Result<String> {
    // Load JJ workspace using native API
    let mut workspace = crate::load_workspace(repo_root)?;

    // Delegate to native implementation
    crate::materialize::publish_checkpoint(
        checkpoint,
        store,
        &mut workspace,
        mapping,
        options,
        repo_root,
    )
}

/// Publish a range of checkpoints to JJ
///
/// Behavior depends on options.compact_range:
/// - If true: Create single JJ commit from last checkpoint (squash)
/// - If false: Create one JJ commit per checkpoint (preserve history)
///
/// Delegates to the native implementation in materialize.rs.
pub fn publish_range(
    checkpoints: Vec<Checkpoint>,
    store: &Store,
    repo_root: &Path,
    mapping: &JjMapping,
    options: &PublishOptions,
) -> Result<Vec<String>> {
    // Delegate to native implementation (workspace loaded internally)
    crate::materialize::publish_range(
        checkpoints,
        store,
        repo_root,
        mapping,
        options,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use journal::{Checkpoint, CheckpointMeta, CheckpointReason};
    use tl_core::{Sha1Hash, Entry, Tree, Store};
    use tempfile::TempDir;
    use std::fs;
    use std::path::PathBuf;

    fn create_test_checkpoint() -> Checkpoint {
        Checkpoint::new(
            None,
            Sha1Hash::from_bytes([1u8; 20]),
            CheckpointReason::FsBatch,
            vec![PathBuf::from("test.txt")],
            CheckpointMeta {
                files_changed: 1,
                bytes_added: 100,
                bytes_removed: 0,
            },
        )
    }

    fn create_test_store_with_tree(temp_dir: &TempDir, include_files: bool) -> Result<(Store, Sha1Hash)> {
        let repo_root = temp_dir.path();
        let store = Store::init(repo_root)?;

        if !include_files {
            // Return empty tree
            let tree = Tree::new();
            let tree_hash = store.write_tree(&tree)?;
            return Ok((store, tree_hash));
        }

        // Create test files
        fs::write(repo_root.join("test.txt"), b"Hello World")?;
        fs::create_dir_all(repo_root.join("dir"))?;
        fs::write(repo_root.join("dir/nested.txt"), b"Nested content")?;

        // Create tree
        let mut tree = Tree::new();
        let blob1 = tl_core::hash::git::hash_blob(b"Hello World");
        let blob2 = tl_core::hash::git::hash_blob(b"Nested content");

        store.blob_store().write_blob(blob1, b"Hello World")?;
        store.blob_store().write_blob(blob2, b"Nested content")?;

        tree.insert(&PathBuf::from("test.txt"), Entry::file(0o644, blob1));
        tree.insert(&PathBuf::from("dir/nested.txt"), Entry::file(0o644, blob2));

        let tree_hash = store.write_tree(&tree)?;

        Ok((store, tree_hash))
    }

    /// Create a test JJ workspace using native jj-lib APIs (no CLI)
    fn create_test_jj_workspace(path: &Path) -> Result<()> {
        use jj_lib::config::StackedConfig;

        // Create minimal config using StackedConfig (required in 0.36.0)
        let config = StackedConfig::with_defaults();
        let user_settings = jj_lib::settings::UserSettings::from_config(config)?;

        // Initialize internal git workspace (avoids 'local' backend)
        jj_lib::workspace::Workspace::init_internal_git(&user_settings, path)?;

        Ok(())
    }

    #[test]
    fn test_materialize_checkpoint_to_dir_creates_files() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let (store, tree_hash) = create_test_store_with_tree(&temp_dir, true)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 2,
                bytes_added: 100,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Verify files were created
        assert!(output_dir.path().join("test.txt").exists());
        assert!(output_dir.path().join("dir/nested.txt").exists());

        // Verify content
        let content1 = fs::read_to_string(output_dir.path().join("test.txt"))?;
        assert_eq!(content1, "Hello World");

        let content2 = fs::read_to_string(output_dir.path().join("dir/nested.txt"))?;
        assert_eq!(content2, "Nested content");

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_materialize_preserves_permissions() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new()?;
        let store = Store::init(temp_dir.path())?;

        // Create executable file
        let mut tree = Tree::new();
        let blob = tl_core::hash::git::hash_blob(b"#!/bin/bash\necho hello");
        store.blob_store().write_blob(blob, b"#!/bin/bash\necho hello")?;
        tree.insert(&PathBuf::from("script.sh"), Entry::file(0o755, blob));
        let tree_hash = store.write_tree(&tree)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 1,
                bytes_added: 20,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Verify permissions
        let metadata = fs::metadata(output_dir.path().join("script.sh"))?;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);

        Ok(())
    }

    #[test]
    fn test_materialize_skips_protected_directories() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Store::init(temp_dir.path())?;

        // Create tree with protected paths
        let mut tree = Tree::new();
        let blob = tl_core::hash::git::hash_blob(b"content");
        store.blob_store().write_blob(blob, b"content")?;

        tree.insert(&PathBuf::from(".tl/config"), Entry::file(0o644, blob));
        tree.insert(&PathBuf::from(".git/HEAD"), Entry::file(0o644, blob));
        tree.insert(&PathBuf::from(".jj/store"), Entry::file(0o644, blob));
        tree.insert(&PathBuf::from("normal.txt"), Entry::file(0o644, blob));

        let tree_hash = store.write_tree(&tree)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 4,
                bytes_added: 100,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Verify only normal.txt was created
        assert!(output_dir.path().join("normal.txt").exists());
        assert!(!output_dir.path().join(".tl").exists());
        assert!(!output_dir.path().join(".git").exists());
        assert!(!output_dir.path().join(".jj").exists());

        Ok(())
    }

    #[test]
    fn test_materialize_handles_nested_paths() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Store::init(temp_dir.path())?;

        // Create deep directory structure
        let mut tree = Tree::new();
        let blob = tl_core::hash::git::hash_blob(b"deep");
        store.blob_store().write_blob(blob, b"deep")?;

        tree.insert(&PathBuf::from("a/b/c/d/e/deep.txt"), Entry::file(0o644, blob));

        let tree_hash = store.write_tree(&tree)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 1,
                bytes_added: 4,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Verify nested file exists
        assert!(output_dir.path().join("a/b/c/d/e/deep.txt").exists());
        let content = fs::read_to_string(output_dir.path().join("a/b/c/d/e/deep.txt"))?;
        assert_eq!(content, "deep");

        Ok(())
    }

    #[test]
    fn test_materialize_with_empty_tree() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let (store, tree_hash) = create_test_store_with_tree(&temp_dir, false)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 0,
                bytes_added: 0,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Verify directory is empty (except for potential . and .. entries)
        let entries: Vec<_> = fs::read_dir(output_dir.path())?.collect();
        assert_eq!(entries.len(), 0);

        Ok(())
    }

    #[test]
    fn test_copy_dir_all_recursive() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        // Create source directory structure
        fs::create_dir_all(src_dir.join("subdir"))?;
        fs::write(src_dir.join("file1.txt"), b"content1")?;
        fs::write(src_dir.join("subdir/file2.txt"), b"content2")?;

        // Copy recursively
        copy_dir_all(&src_dir, &dst_dir)?;

        // Verify structure
        assert!(dst_dir.join("file1.txt").exists());
        assert!(dst_dir.join("subdir/file2.txt").exists());

        let content1 = fs::read_to_string(dst_dir.join("file1.txt"))?;
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(dst_dir.join("subdir/file2.txt"))?;
        assert_eq!(content2, "content2");

        Ok(())
    }

    #[test]
    fn test_publish_range_compact_mode() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let (store, tree_hash) = create_test_store_with_tree(&temp_dir, true)?;

        // Initialize JJ workspace using native API
        create_test_jj_workspace(temp_dir.path())?;

        let mapping = JjMapping::open(&temp_dir.path().join(".tl"))?;

        // Create checkpoints
        let cp1 = Checkpoint::new(None, tree_hash, CheckpointReason::FsBatch, vec![], CheckpointMeta::default());
        let cp2 = Checkpoint::new(Some(cp1.id), tree_hash, CheckpointReason::FsBatch, vec![], CheckpointMeta::default());

        let options = PublishOptions {
            auto_pin: None,
            message_options: crate::materialize::CommitMessageOptions::default(),
            compact_range: true,
            accumulated_paths: None,
        };

        let commit_ids = publish_range(vec![cp1.clone(), cp2.clone()], &store, temp_dir.path(), &mapping, &options)?;

        // In compact mode, should create single commit
        assert_eq!(commit_ids.len(), 1);

        // Verify mapping for last checkpoint only
        assert!(mapping.get_jj_commit(cp2.id)?.is_some());

        Ok(())
    }

    #[test]
    fn test_publish_range_expand_mode() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let (store, tree_hash) = create_test_store_with_tree(&temp_dir, true)?;

        // Initialize JJ workspace using native API
        create_test_jj_workspace(temp_dir.path())?;

        let mapping = JjMapping::open(&temp_dir.path().join(".tl"))?;

        let cp1 = Checkpoint::new(None, tree_hash, CheckpointReason::FsBatch, vec![], CheckpointMeta::default());
        let cp2 = Checkpoint::new(Some(cp1.id), tree_hash, CheckpointReason::FsBatch, vec![], CheckpointMeta::default());

        let options = PublishOptions {
            auto_pin: None,
            message_options: crate::materialize::CommitMessageOptions::default(),
            compact_range: false, // Expand mode
            accumulated_paths: None,
        };

        let commit_ids = publish_range(vec![cp1.clone(), cp2.clone()], &store, temp_dir.path(), &mapping, &options)?;

        // In expand mode, should create one commit per checkpoint
        assert_eq!(commit_ids.len(), 2);

        // Verify both checkpoints mapped
        assert!(mapping.get_jj_commit(cp1.id)?.is_some());
        assert!(mapping.get_jj_commit(cp2.id)?.is_some());

        Ok(())
    }

    #[test]
    fn test_publish_range_empty_list() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Store::init(temp_dir.path())?;

        // Initialize JJ workspace using native API
        create_test_jj_workspace(temp_dir.path())?;

        let mapping = JjMapping::open(&temp_dir.path().join(".tl"))?;

        let options = PublishOptions {
            auto_pin: None,
            message_options: crate::materialize::CommitMessageOptions::default(),
            compact_range: false,
            accumulated_paths: None,
        };

        let commit_ids = publish_range(vec![], &store, temp_dir.path(), &mapping, &options)?;

        // Empty input should return empty output
        assert_eq!(commit_ids.len(), 0);

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_materialize_with_symlinks() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Store::init(temp_dir.path())?;

        // Create symlink entry
        let target = PathBuf::from("../target");
        let target_bytes = target.to_string_lossy();
        let blob = tl_core::hash::git::hash_blob(target_bytes.as_bytes());
        store.blob_store().write_blob(blob, target_bytes.as_bytes())?;

        let mut tree = Tree::new();
        tree.insert(&PathBuf::from("link"), Entry::symlink(blob));

        let tree_hash = store.write_tree(&tree)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 1,
                bytes_added: 0,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Note: Currently materialize doesn't restore symlinks, only regular files
        // This test documents current behavior
        // If symlink support is added later, update this test

        Ok(())
    }

    #[test]
    fn test_publish_checkpoint_with_timestamp() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let (store, tree_hash) = create_test_store_with_tree(&temp_dir, true)?;

        // Initialize JJ workspace using native API
        create_test_jj_workspace(temp_dir.path())?;

        let mapping = JjMapping::open(&temp_dir.path().join(".tl"))?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta::default(),
        );

        let options = PublishOptions {
            auto_pin: None,
            message_options: crate::materialize::CommitMessageOptions {
                template: None,
                include_metadata: true,
                include_files: true,
                max_files_shown: 10,
            },
            compact_range: false,
            accumulated_paths: None,
        };

        let commit_id = publish_checkpoint(&checkpoint, &store, temp_dir.path(), &mapping, &options)?;

        // Verify commit was created
        assert!(!commit_id.is_empty());

        // Verify timestamp is preserved in checkpoint (ULID includes timestamp)
        let timestamp_ms = checkpoint.id.timestamp_ms();
        assert!(timestamp_ms > 0);

        Ok(())
    }

    #[test]
    fn test_materialize_creates_parent_directories() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let store = Store::init(temp_dir.path())?;

        // Create file in nested directory that doesn't exist yet
        let mut tree = Tree::new();
        let blob = tl_core::hash::git::hash_blob(b"nested");
        store.blob_store().write_blob(blob, b"nested")?;
        tree.insert(&PathBuf::from("does/not/exist/file.txt"), Entry::file(0o644, blob));

        let tree_hash = store.write_tree(&tree)?;

        let checkpoint = Checkpoint::new(
            None,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![],
            CheckpointMeta {
                files_changed: 1,
                bytes_added: 6,
                bytes_removed: 0,
            },
        );

        let output_dir = TempDir::new()?;
        materialize_checkpoint_to_dir(&checkpoint, &store, output_dir.path())?;

        // Verify parent directories were created
        assert!(output_dir.path().join("does/not/exist/file.txt").exists());

        Ok(())
    }
}
