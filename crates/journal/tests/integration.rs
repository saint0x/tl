//! Integration tests for journal crate

use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal, PathMap};
use core::{Sha1Hash, Entry, Store, Tree, hash::hash_bytes};
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_full_checkpoint_lifecycle() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;

    // Initialize store (creates .tl directory structure)
    let store = Store::init(temp_dir.path())?;
    let tl_dir = temp_dir.path().join(".tl");
    let journal = Journal::open(&tl_dir)?;

    // Create initial tree
    let mut tree = Tree::new();
    let data1 = b"initial content";
    let hash1 = hash_bytes(data1);
    let entry1 = Entry::file(0o644, hash1);
    tree.insert(&PathBuf::from("file1.txt"), entry1);

    // Store tree
    store.write_tree(&tree)?;
    let tree_hash = tree.hash();

    // Create checkpoint
    let meta = CheckpointMeta {
        files_changed: 1,
        bytes_added: data1.len() as u64,
        bytes_removed: 0,
    };

    let checkpoint = Checkpoint::new(
        None,
        tree_hash,
        CheckpointReason::Manual,
        vec![PathBuf::from("file1.txt")],
        meta,
    );

    // Append to journal
    journal.append(&checkpoint)?;

    // Verify we can retrieve it
    let latest = journal.latest()?.unwrap();
    assert_eq!(latest.id, checkpoint.id);
    assert_eq!(latest.root_tree, tree_hash);

    // Create PathMap
    let pathmap = PathMap::from_tree(&tree, tree_hash);
    assert_eq!(pathmap.len(), 1);

    // Save PathMap
    let pathmap_path = tl_dir.join("state/pathmap.bin");
    std::fs::create_dir_all(tl_dir.join("state"))?;
    pathmap.save(&pathmap_path)?;

    // Load and verify
    let loaded_pathmap = PathMap::load(&pathmap_path)?;
    assert_eq!(loaded_pathmap.len(), 1);
    assert_eq!(loaded_pathmap.root_tree, tree_hash);

    Ok(())
}

#[test]
fn test_checkpoint_chain() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;

    let store = Store::init(temp_dir.path())?;
    let tl_dir = temp_dir.path().join(".tl");
    let journal = Journal::open(&tl_dir)?;

    // Create chain of 10 checkpoints
    let mut parent_id = None;
    let mut last_tree_hash = Sha1Hash::from_bytes([0u8; 20]);

    for i in 0..10 {
        let mut tree = Tree::new();
        let data = format!("content {}", i);
        let hash = hash_bytes(data.as_bytes());
        let entry = Entry::file(0o644, hash);
        tree.insert(&PathBuf::from(format!("file{}.txt", i)), entry);

        store.write_tree(&tree)?;
        let tree_hash = tree.hash();

        let meta = CheckpointMeta {
            files_changed: 1,
            bytes_added: data.len() as u64,
            bytes_removed: 0,
        };

        let checkpoint = Checkpoint::new(
            parent_id,
            tree_hash,
            CheckpointReason::FsBatch,
            vec![PathBuf::from(format!("file{}.txt", i))],
            meta,
        );

        journal.append(&checkpoint)?;
        parent_id = Some(checkpoint.id);
        last_tree_hash = tree_hash;
    }

    // Verify count
    assert_eq!(journal.count(), 10);

    // Verify latest has correct parent
    let latest = journal.latest()?.unwrap();
    assert!(latest.parent.is_some());
    assert_eq!(latest.root_tree, last_tree_hash);

    Ok(())
}

#[test]
fn test_pathmap_recovery() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;

    let store = Store::init(temp_dir.path())?;
    let tl_dir = temp_dir.path().join(".tl");
    let journal = Journal::open(&tl_dir)?;

    // Create tree and checkpoint
    let mut tree = Tree::new();
    let hash = hash_bytes(b"test");
    tree.insert(&PathBuf::from("test.txt"), Entry::file(0o644, hash));

    store.write_tree(&tree)?;
    let tree_hash = tree.hash();

    let meta = CheckpointMeta {
        files_changed: 1,
        bytes_added: 4,
        bytes_removed: 0,
    };

    let checkpoint = Checkpoint::new(
        None,
        tree_hash,
        CheckpointReason::Manual,
        vec![PathBuf::from("test.txt")],
        meta,
    );

    journal.append(&checkpoint)?;

    // Test recovery: rebuild PathMap from journal
    let recovered_tree = store.read_tree(checkpoint.root_tree)?;
    let recovered_pathmap = PathMap::from_tree(&recovered_tree, tree_hash);

    assert_eq!(recovered_pathmap.len(), 1);
    assert_eq!(recovered_pathmap.root_tree, tree_hash);

    Ok(())
}
