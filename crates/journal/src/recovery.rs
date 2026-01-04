//! Crash recovery and consistency verification
//!
//! Ensures data integrity after unclean shutdown

use anyhow::Result;
use core::Store;
use crate::{Journal, PathMap};
use std::path::Path;

/// Verify journal consistency and recover from crashes
pub fn recover_on_startup(
    tl_dir: &Path,
    journal: &Journal,
    store: &Store,
) -> Result<()> {
    // Step 1: Clean up incomplete writes in .tl/tmp/
    cleanup_temp_files(tl_dir)?;

    // Step 2: Verify journal consistency
    verify_journal_integrity(journal)?;

    // Step 3: Verify PathMap matches HEAD checkpoint
    verify_pathmap_consistency(tl_dir, journal, store)?;

    Ok(())
}

/// Delete incomplete checkpoint writes in .tl/tmp/
fn cleanup_temp_files(tl_dir: &Path) -> Result<()> {
    let tmp_dir = tl_dir.join("tmp");

    // If tmp directory doesn't exist, nothing to clean
    if !tmp_dir.exists() {
        return Ok(());
    }

    // Remove all files in tmp directory
    for entry in std::fs::read_dir(&tmp_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            std::fs::remove_file(&path)?;
            eprintln!("Recovery: Removed incomplete write: {}", path.display());
        } else if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
            eprintln!("Recovery: Removed incomplete directory: {}", path.display());
        }
    }

    Ok(())
}

/// Verify journal has valid checkpoints
fn verify_journal_integrity(journal: &Journal) -> Result<()> {
    let count = journal.count();

    if count == 0 {
        // Empty journal is valid (fresh repository)
        return Ok(());
    }

    // Try to read the latest checkpoint
    let latest = journal.latest()?;

    if latest.is_none() {
        return Err(anyhow::anyhow!(
            "Journal corruption: Count is {} but no latest checkpoint found",
            count
        ));
    }

    // Verify we can retrieve checkpoint by ID
    let checkpoint = latest.unwrap();
    let retrieved = journal.get(&checkpoint.id)?;

    if retrieved.is_none() {
        return Err(anyhow::anyhow!(
            "Journal corruption: Latest checkpoint {} not retrievable",
            checkpoint.id
        ));
    }

    eprintln!("Recovery: Journal verified ({} checkpoints)", count);
    Ok(())
}

/// Verify PathMap is consistent with HEAD checkpoint
fn verify_pathmap_consistency(
    tl_dir: &Path,
    journal: &Journal,
    store: &Store,
) -> Result<()> {
    let pathmap_path = tl_dir.join("state/pathmap.bin");

    // If no pathmap exists, nothing to verify
    if !pathmap_path.exists() {
        eprintln!("Recovery: No pathmap found (will be created on first checkpoint)");
        return Ok(());
    }

    // Load the pathmap
    let pathmap = match PathMap::load(&pathmap_path) {
        Ok(pm) => pm,
        Err(e) => {
            eprintln!("Recovery: PathMap corrupted, rebuilding: {}", e);
            rebuild_pathmap(tl_dir, journal, store)?;
            return Ok(());
        }
    };

    // Get the latest checkpoint
    let latest = match journal.latest()? {
        Some(cp) => cp,
        None => {
            // No checkpoints but pathmap exists - something is wrong
            eprintln!("Recovery: PathMap exists but no checkpoints, removing pathmap");
            std::fs::remove_file(&pathmap_path)?;
            return Ok(());
        }
    };

    // Load the tree for the HEAD checkpoint
    let head_tree = match store.read_tree(latest.root_tree) {
        Ok(tree) => tree,
        Err(e) => {
            eprintln!("Recovery: Cannot read HEAD tree, rebuilding pathmap: {}", e);
            rebuild_pathmap(tl_dir, journal, store)?;
            return Ok(());
        }
    };

    // Verify pathmap matches HEAD tree
    if !pathmap_matches_tree(&pathmap, &head_tree) {
        eprintln!("Recovery: PathMap mismatch with HEAD, rebuilding");
        rebuild_pathmap(tl_dir, journal, store)?;
        return Ok(());
    }

    eprintln!("Recovery: PathMap verified");
    Ok(())
}

/// Check if PathMap matches a Tree
fn pathmap_matches_tree(pathmap: &PathMap, tree: &core::Tree) -> bool {
    // Collect tree entries to check count
    let tree_entries: Vec<_> = tree.entries_with_paths().collect();

    // Quick check: same number of entries
    if pathmap.len() != tree_entries.len() {
        return false;
    }

    // Deep check: all entries match
    for (tree_path_bytes, tree_entry) in tree_entries {
        match pathmap.get_by_bytes(tree_path_bytes) {
            Some(pm_entry) if pm_entry == tree_entry => continue,
            _ => return false,
        }
    }

    true
}

/// Rebuild PathMap from journal (last resort)
fn rebuild_pathmap(
    tl_dir: &Path,
    journal: &Journal,
    store: &Store,
) -> Result<()> {
    let pathmap_path = tl_dir.join("state/pathmap.bin");

    // Get the latest checkpoint
    let latest = match journal.latest()? {
        Some(cp) => cp,
        None => {
            // No checkpoints - just delete the pathmap
            if pathmap_path.exists() {
                std::fs::remove_file(&pathmap_path)?;
            }
            return Ok(());
        }
    };

    // Load the tree for the HEAD checkpoint
    let head_tree = store.read_tree(latest.root_tree)?;

    // Create new PathMap from tree
    let new_pathmap = PathMap::from_tree(&head_tree, latest.root_tree);

    // Save the rebuilt pathmap
    new_pathmap.save(&pathmap_path)?;

    eprintln!("Recovery: PathMap rebuilt from HEAD checkpoint {}", latest.id);
    Ok(())
}
