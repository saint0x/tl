//! Incremental tree update algorithm
//!
//! The performance linchpin: update tree from dirty paths without full rescan

use anyhow::Result;
use core::{hash, Sha1Hash, Entry, Store, Tree};
use crate::PathMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Update a tree incrementally from a set of dirty paths
///
/// This is the core algorithm that enables < 10ms checkpoint creation
pub fn incremental_update(
    base_map: &PathMap,
    dirty_paths: Vec<&Path>,
    repo_root: &Path,
    store: &Store,
) -> Result<(PathMap, Tree, Sha1Hash)> {
    // Step 1: Normalize and deduplicate dirty paths
    let normalized = normalize_dirty_paths(dirty_paths, repo_root)?;

    // Step 2: Clone base map for modifications
    let mut new_map = base_map.clone();

    // Step 3: Reconcile each dirty path
    for path in normalized {
        reconcile_path(&mut new_map, &path, repo_root, store)?;
    }

    // Step 4: Build tree from updated map
    let tree = build_tree_from_map(&new_map)?;

    // Step 5: Compute tree hash
    let tree_hash = tree.hash();

    // Step 6: Store tree in content-addressed storage
    store.write_tree(&tree)?;

    Ok((new_map, tree, tree_hash))
}

/// Normalize and deduplicate dirty paths
fn normalize_dirty_paths(paths: Vec<&Path>, _repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut normalized = HashSet::new();

    for path in paths {
        // Paths from watcher are already repo-relative
        let path_buf = path.to_path_buf();

        // Skip .tl/ and .git/ (should already be filtered by watcher, but double-check)
        if core::store::should_ignore(&path_buf) {
            continue;
        }

        // Normalize path representation
        if let Ok(normalized_path) = core::store::normalize_path(&path_buf) {
            normalized.insert(normalized_path);
        }
    }

    Ok(normalized.into_iter().collect())
}

/// Reconcile a single path (update or remove from map)
fn reconcile_path(
    map: &mut PathMap,
    path: &Path,
    repo_root: &Path,
    store: &Store,
) -> Result<()> {
    let abs_path = repo_root.join(path);

    // Attempt to get file metadata
    let metadata = match std::fs::symlink_metadata(&abs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Case 2: File deleted - remove from map
            map.update(path, None);
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Permission issue - skip this file with warning
            eprintln!("Warning: Permission denied for {}: {}", path.display(), e);
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to stat {}: {}", path.display(), e));
        }
    };

    // Case 3: Symlink
    if metadata.is_symlink() {
        return reconcile_symlink(map, path, &abs_path, store);
    }

    // Case 1: Regular file
    if metadata.is_file() {
        return reconcile_file(map, path, &abs_path, metadata, store);
    }

    // Ignore directories (implicit in flat tree structure)
    Ok(())
}

/// Verify that a file is stable during read (double-stat verification)
///
/// This prevents race conditions where a file is modified during read
fn verify_stable_read(path: &Path) -> Result<Vec<u8>> {
    // First stat
    let stat1 = std::fs::metadata(path)?;
    let mtime1 = stat1.modified()?;

    // Read file contents
    let data = std::fs::read(path)?;

    // Second stat
    let stat2 = std::fs::metadata(path)?;
    let mtime2 = stat2.modified()?;

    // Verify file wasn't modified during read
    if mtime1 != mtime2 {
        return Err(anyhow::anyhow!(
            "File modified during read: {} (will be requeued)",
            path.display()
        ));
    }

    Ok(data)
}

/// Reconcile a regular file
fn reconcile_file(
    map: &mut PathMap,
    path: &Path,
    abs_path: &Path,
    metadata: std::fs::Metadata,
    store: &Store,
) -> Result<()> {
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

    // Hash the file with stability verification (double-stat pattern)
    let blob_hash = hash::hash_file_stable(abs_path, 3)?;

    // Check if we already have this blob
    if !store.blob_store().has_blob(blob_hash) {
        // Read file with double-stat verification to ensure stable read
        let contents = verify_stable_read(abs_path)?;
        store.blob_store().write_blob(blob_hash, &contents)?;
    }

    // Check if entry exists and mode has changed (permission-only change detection)
    if let Some(existing_entry) = map.get(path) {
        // Only update if hash OR mode changed
        if existing_entry.blob_hash == blob_hash && existing_entry.mode == mode {
            // No change - skip update
            return Ok(());
        }
        // Otherwise, update with new entry (hash or mode changed)
    }

    // Update map with new entry
    let entry = Entry::file(mode, blob_hash);
    map.update(path, Some(entry));

    Ok(())
}

/// Reconcile a symlink
fn reconcile_symlink(
    map: &mut PathMap,
    path: &Path,
    abs_path: &Path,
    store: &Store,
) -> Result<()> {
    // Read symlink target
    let target = std::fs::read_link(abs_path)?;
    let target_bytes = target.to_string_lossy();

    // Hash the target path (not the content it points to)
    let blob_hash = hash::hash_bytes(target_bytes.as_bytes());

    // Check if symlink target has changed
    if let Some(existing_entry) = map.get(path) {
        if existing_entry.blob_hash == blob_hash {
            // Symlink target unchanged - skip update
            return Ok(());
        }
        // Otherwise, target changed - update entry
    }

    // Store the target as a blob
    if !store.blob_store().has_blob(blob_hash) {
        store
            .blob_store()
            .write_blob(blob_hash, target_bytes.as_bytes())?;
    }

    // Create symlink entry
    let entry = Entry::symlink(blob_hash);
    map.update(path, Some(entry));

    Ok(())
}

/// Build a tree from PathMap
fn build_tree_from_map(map: &PathMap) -> Result<Tree> {
    let mut tree = Tree::new();

    for (path_bytes, entry) in map.entries() {
        // Convert SmallVec bytes back to Path
        let path_str = std::str::from_utf8(path_bytes.as_slice())?;
        let path_buf = PathBuf::from(path_str);

        tree.insert(&path_buf, entry.clone());
    }

    Ok(tree)
}
