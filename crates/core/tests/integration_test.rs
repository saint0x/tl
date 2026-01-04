//! Integration tests for timelapse core functionality

use core::hash::git::hash_blob;
use core::store::Store;
use core::tree::{Entry, Tree};
use std::path::Path;

#[test]
fn test_full_storage_pipeline() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let repo_root = temp_dir.path();

    // Initialize store
    let store = Store::init(repo_root)?;

    // Write some blobs
    let blob1_data = b"This is the content of file1.txt";
    let blob2_data = b"This is the content of file2.txt";
    let blob3_data = b"This is the content of file3.txt with more data to potentially trigger compression";

    let hash1 = hash_blob(blob1_data);
    let hash2 = hash_blob(blob2_data);
    let hash3 = hash_blob(blob3_data);

    store.blob_store().write_blob(hash1, blob1_data)?;
    store.blob_store().write_blob(hash2, blob2_data)?;
    store.blob_store().write_blob(hash3, blob3_data)?;

    // Build a tree
    let mut tree = Tree::new();
    tree.insert(Path::new("file1.txt"), Entry::file(0o644, hash1));
    tree.insert(Path::new("file2.txt"), Entry::file(0o644, hash2));
    tree.insert(Path::new("src/file3.txt"), Entry::file(0o644, hash3));

    // Write the tree
    let tree_hash = store.write_tree(&tree)?;

    // Read the tree back
    let read_tree = store.read_tree(tree_hash)?;

    // Verify tree structure
    assert_eq!(read_tree.len(), 3);
    assert!(read_tree.get(Path::new("file1.txt")).is_some());
    assert!(read_tree.get(Path::new("file2.txt")).is_some());
    assert!(read_tree.get(Path::new("src/file3.txt")).is_some());

    // Verify blob data
    let read_blob1 = store.blob_store().read_blob(hash1)?;
    let read_blob2 = store.blob_store().read_blob(hash2)?;
    let read_blob3 = store.blob_store().read_blob(hash3)?;

    assert_eq!(read_blob1.as_slice(), blob1_data);
    assert_eq!(read_blob2.as_slice(), blob2_data);
    assert_eq!(read_blob3.as_slice(), blob3_data);

    Ok(())
}

#[test]
fn test_store_persistence() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let repo_root = temp_dir.path();

    // Initialize and write data
    {
        let store = Store::init(repo_root)?;

        let data = b"persistent data";
        let hash = hash_blob(data);

        store.blob_store().write_blob(hash, data)?;

        let mut tree = Tree::new();
        tree.insert(Path::new("file.txt"), Entry::file(0o644, hash));

        let _tree_hash = store.write_tree(&tree)?;
    }

    // Reopen store and verify data persists
    {
        let store = Store::open(repo_root)?;

        let data = b"persistent data";
        let hash = hash_blob(data);

        // Blob should still be readable
        let blob = store.blob_store().read_blob(hash)?;
        assert_eq!(blob.as_slice(), data);
    }

    Ok(())
}

#[test]
fn test_large_tree() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let repo_root = temp_dir.path();

    let store = Store::init(repo_root)?;

    // Create 100 files
    let mut tree = Tree::new();
    for i in 0..100 {
        let data = format!("content of file {}", i);
        let hash = hash_blob(data.as_bytes());

        store.blob_store().write_blob(hash, data.as_bytes())?;

        let path = format!("files/file{:03}.txt", i);
        tree.insert(Path::new(&path), Entry::file(0o644, hash));
    }

    // Write and read the tree
    let tree_hash = store.write_tree(&tree)?;
    let read_tree = store.read_tree(tree_hash)?;

    // Verify all 100 entries
    assert_eq!(read_tree.len(), 100);

    for i in 0..100 {
        let path = format!("files/file{:03}.txt", i);
        assert!(read_tree.get(Path::new(&path)).is_some());
    }

    Ok(())
}

#[test]
fn test_tree_diff_integration() -> anyhow::Result<()> {
    use core::tree::TreeDiff;

    let temp_dir = tempfile::tempdir()?;
    let repo_root = temp_dir.path();

    let store = Store::init(repo_root)?;

    // Create initial tree
    let hash1 = hash_blob(b"content1");
    let hash2 = hash_blob(b"content2");
    let hash3 = hash_blob(b"modified");

    store.blob_store().write_blob(hash1, b"content1")?;
    store.blob_store().write_blob(hash2, b"content2")?;
    store.blob_store().write_blob(hash3, b"modified")?;

    let mut tree1 = Tree::new();
    tree1.insert(Path::new("file1.txt"), Entry::file(0o644, hash1));
    tree1.insert(Path::new("file2.txt"), Entry::file(0o644, hash2));

    let tree1_hash = store.write_tree(&tree1)?;

    // Create modified tree
    let mut tree2 = Tree::new();
    tree2.insert(Path::new("file1.txt"), Entry::file(0o644, hash3)); // Modified
    tree2.insert(Path::new("file3.txt"), Entry::file(0o644, hash1)); // Added

    let tree2_hash = store.write_tree(&tree2)?;

    // Compute diff
    let diff = TreeDiff::diff(&tree1, &tree2);

    assert_eq!(diff.added.len(), 1); // file3.txt
    assert_eq!(diff.removed.len(), 1); // file2.txt
    assert_eq!(diff.modified.len(), 1); // file1.txt

    // Verify both trees are stored
    let read_tree1 = store.read_tree(tree1_hash)?;
    let read_tree2 = store.read_tree(tree2_hash)?;

    assert_eq!(read_tree1.hash(), tree1.hash());
    assert_eq!(read_tree2.hash(), tree2.hash());

    Ok(())
}

#[test]
fn test_concurrent_operations() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::thread;

    let temp_dir = tempfile::tempdir()?;
    let repo_root = temp_dir.path().to_path_buf();

    let store = Arc::new(Store::init(&repo_root)?);

    // Spawn multiple threads writing blobs concurrently
    let mut handles = vec![];

    for i in 0..10 {
        let store_clone = Arc::clone(&store);
        let handle = thread::spawn(move || -> anyhow::Result<()> {
            let data = format!("thread {} data", i);
            let hash = hash_blob(data.as_bytes());
            store_clone.blob_store().write_blob(hash, data.as_bytes())?;
            Ok(())
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap()?;
    }

    // Verify all blobs were written
    for i in 0..10 {
        let data = format!("thread {} data", i);
        let hash = hash_blob(data.as_bytes());
        let blob = store.blob_store().read_blob(hash)?;
        assert_eq!(blob.as_slice(), data.as_bytes());
    }

    Ok(())
}
