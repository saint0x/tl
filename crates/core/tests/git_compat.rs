/// Git Compatibility Validation Tests
///
/// These tests validate that our SHA-1 implementation produces
/// Git-compatible objects that Git itself can read and write.
///
/// Key validations:
/// 1. Blob hashes match what Git produces
/// 2. Tree hashes match what Git produces
/// 3. Object format is readable by Git
/// 4. Compression format is standard zlib

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use core::{
    hash::{git, Sha1Hash},
    BlobStore, Entry, Tree, Store,
};

/// Test that blob hash matches what Git produces
///
/// Validation: Use `git hash-object` to verify our hash implementation
#[test]
fn test_blob_hash_matches_git() {
    let temp_dir = TempDir::new().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    let content = b"Hello, Git!\n";

    fs::write(&test_file, content).unwrap();

    // Get hash from our implementation
    let our_hash = git::hash_blob(content);
    let our_hex = our_hash.to_hex();

    // Get hash from actual Git
    let output = Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["hash-object", test_file.to_str().unwrap()])
        .output()
        .expect("Git must be installed for this test");

    assert!(output.status.success(), "git hash-object failed");

    let git_hex = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .to_string();

    assert_eq!(
        our_hex, git_hex,
        "Our hash doesn't match Git's hash!\nOurs: {}\nGit:  {}",
        our_hex, git_hex
    );
}

/// Test blob format with multiple content types
#[test]
fn test_blob_hash_various_contents() {
    let large_vec = vec![0u8; 1024];
    let test_cases: Vec<(&[u8], &str)> = vec![
        (b"" as &[u8], "empty file"),
        (b"a", "single byte"),
        (b"Hello, World!", "simple text"),
        (b"\0\0\0", "binary data with nulls"),
        (b"Line 1\nLine 2\nLine 3\n", "multiline text"),
        (&large_vec[..], "1KB of zeros"),
    ];

    let temp_dir = TempDir::new().unwrap();

    for (content, description) in test_cases {
        let test_file = temp_dir.path().join("test.bin");
        fs::write(&test_file, content).unwrap();

        // Our hash
        let our_hash = git::hash_blob(content);
        let our_hex = our_hash.to_hex();

        // Git hash
        let output = Command::new("git")
            .current_dir(temp_dir.path())
            .args(&["hash-object", test_file.to_str().unwrap()])
            .output()
            .unwrap();

        let git_hex = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .to_string();

        assert_eq!(
            our_hex, git_hex,
            "Hash mismatch for {}: ours={}, git={}",
            description, our_hex, git_hex
        );
    }
}

/// Test that blob write/read roundtrip works
#[test]
fn test_blob_write_read_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let store_path = temp_dir.path();
    let blob_store = BlobStore::new(store_path.to_path_buf());

    let large_vec = vec![42u8; 4096];
    let contents: Vec<&[u8]> = vec![
        b"test content" as &[u8],
        b"",
        b"binary\0data\0with\0nulls",
        &large_vec[..],
    ];

    for content in contents {
        // Compute hash
        let hash = git::hash_blob(content);

        // Write blob
        blob_store.write_blob(hash, content).unwrap();

        // Read back
        let read_content = blob_store.read_blob(hash).unwrap();

        assert_eq!(
            content, &read_content[..],
            "Roundtrip failed for content of length {}",
            content.len()
        );
    }
}

/// Test that Git can read our blob objects
#[test]
fn test_git_can_read_our_blobs() {
    let temp_dir = TempDir::new().unwrap();

    // Initialize Git repo
    Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["init", "--quiet"])
        .status()
        .unwrap();

    let git_dir = temp_dir.path().join(".git");
    let blob_store = BlobStore::new(git_dir.clone());

    // Write blob using our implementation
    let content = b"Test content for Git to read\n";
    let hash = git::hash_blob(content);
    blob_store.write_blob(hash, content).unwrap();

    let hex = hash.to_hex();

    // Verify Git can read it with cat-file
    let output = Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["cat-file", "-p", &hex])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Git failed to read our blob object: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        content,
        &output.stdout[..],
        "Git read different content than we wrote"
    );

    // Verify Git sees it as a blob
    let output = Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["cat-file", "-t", &hex])
        .output()
        .unwrap();

    assert_eq!(
        "blob\n",
        String::from_utf8(output.stdout).unwrap(),
        "Git doesn't recognize our object as a blob"
    );
}

/// Test tree hash format with our Tree API
#[test]
fn test_tree_hash_format() {
    let _temp_dir = TempDir::new().unwrap();

    // Create a simple tree with one file
    let file_content = b"file content";
    let file_hash = git::hash_blob(file_content);

    let mut tree = Tree::new();
    tree.insert(
        Path::new("test.txt"),
        Entry::file(0o100644, file_hash),
    );

    // Serialize and compute hash
    let serialized = tree.serialize();

    // Hash should be deterministic - create another identical tree
    let mut tree2 = Tree::new();
    tree2.insert(
        Path::new("test.txt"),
        Entry::file(0o100644, file_hash),
    );

    let serialized2 = tree2.serialize();

    assert_eq!(serialized, serialized2, "Tree serialization is not deterministic");
}

/// Test tree with multiple entries (sorted order)
#[test]
fn test_tree_entry_sorting() {
    let file_hash = git::hash_blob(b"content");

    // Create tree with entries in different orders
    let mut tree1 = Tree::new();
    tree1.insert(Path::new("a.txt"), Entry::file(0o100644, file_hash));
    tree1.insert(Path::new("c.txt"), Entry::file(0o100644, file_hash));
    tree1.insert(Path::new("b.txt"), Entry::file(0o100644, file_hash));

    let mut tree2 = Tree::new();
    tree2.insert(Path::new("b.txt"), Entry::file(0o100644, file_hash));
    tree2.insert(Path::new("a.txt"), Entry::file(0o100644, file_hash));
    tree2.insert(Path::new("c.txt"), Entry::file(0o100644, file_hash));

    let ser1 = tree1.serialize();
    let ser2 = tree2.serialize();

    assert_eq!(
        ser1, ser2,
        "Tree serialization should be same regardless of insertion order (Git trees are sorted)"
    );
}

/// Test executable file mode handling
#[test]
fn test_executable_file_mode() {
    let script_content = b"#!/bin/bash\necho hello\n";
    let script_hash = git::hash_blob(script_content);

    // Create tree with executable file
    let mut tree = Tree::new();
    tree.insert(
        Path::new("script.sh"),
        Entry::executable(script_hash),
    );

    // Should serialize successfully
    let _serialized = tree.serialize();
}

/// Test symlink handling
#[test]
fn test_symlink_in_tree() {
    // Symlink target is stored as blob content
    let target = b"../some/path";
    let link_hash = git::hash_blob(target);

    let mut tree = Tree::new();
    tree.insert(
        Path::new("mylink"),
        Entry::symlink(link_hash),
    );

    let _serialized = tree.serialize();
}

/// Test tree write and Git can read it
#[test]
fn test_git_can_read_our_trees() {
    use std::io::Write;

    let temp_dir = TempDir::new().unwrap();

    // Initialize Git repo
    Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["init", "--quiet"])
        .status()
        .unwrap();

    let git_dir = temp_dir.path().join(".git");
    let objects_dir = git_dir.join("objects");

    // Create blob store
    let blob_store = BlobStore::new(git_dir.clone());

    // Write a blob first
    let file_content = b"Hello from tree test\n";
    let file_hash = git::hash_blob(file_content);
    blob_store.write_blob(file_hash, file_content).unwrap();

    // Create tree
    let mut tree = Tree::new();
    tree.insert(
        Path::new("hello.txt"),
        Entry::file(0o100644, file_hash),
    );

    // Serialize tree and write to Git objects directory
    let serialized = tree.serialize();
    let tree_hash = tree.hash();
    let tree_hex = tree_hash.to_hex();

    // Write tree to Git objects directory
    let prefix = &tree_hex[..2];
    let suffix = &tree_hex[2..];
    let tree_dir = objects_dir.join(prefix);
    fs::create_dir_all(&tree_dir).unwrap();
    let tree_path = tree_dir.join(suffix);
    fs::File::create(&tree_path).unwrap().write_all(&serialized).unwrap();

    // Verify Git recognizes it as a tree
    let output = Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["cat-file", "-t", &tree_hex])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Git failed to read our tree: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        "tree\n",
        String::from_utf8(output.stdout).unwrap(),
        "Git doesn't recognize our object as a tree"
    );

    // List tree contents via Git
    let output = Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["ls-tree", &tree_hex])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Git failed to list tree: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let listing = String::from_utf8(output.stdout).unwrap();
    assert!(listing.contains("hello.txt"), "Tree should contain hello.txt");
    assert!(listing.contains("100644"), "File should have mode 100644");
}

/// Test hash collision resistance (basic sanity check)
#[test]
fn test_different_content_different_hash() {
    let content1 = b"content 1";
    let content2 = b"content 2";

    let hash1 = git::hash_blob(content1);
    let hash2 = git::hash_blob(content2);

    assert_ne!(hash1, hash2, "Different content should have different hashes");
}

/// Test empty blob
#[test]
fn test_empty_blob() {
    let empty = b"";
    let hash = git::hash_blob(empty);

    // Git's hash for empty blob is well-known: e69de29bb2d1d6434b8b29ae775ad8c2e48c5391
    assert_eq!(
        hash.to_hex(),
        "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        "Empty blob hash should match Git's well-known value"
    );
}

/// Test large blob (stress test)
#[test]
fn test_large_blob() {
    let temp_dir = TempDir::new().unwrap();
    let blob_store = BlobStore::new(temp_dir.path().to_path_buf());

    // 1 MB blob
    let large_content = vec![42u8; 1024 * 1024];
    let hash = git::hash_blob(&large_content);

    // Write and read
    blob_store.write_blob(hash, &large_content).unwrap();
    let read_back = blob_store.read_blob(hash).unwrap();

    assert_eq!(large_content.len(), read_back.len());
    assert_eq!(large_content, read_back);
}

/// Test tree with subdirectory
#[test]
fn test_tree_with_subdirectory() {
    let temp_dir = TempDir::new().unwrap();
    let store = Store::init(temp_dir.path()).unwrap();

    let file_hash = git::hash_blob(b"file in subdir");

    // Create subtree
    let mut subtree = Tree::new();
    subtree.insert(
        Path::new("file.txt"),
        Entry::file(0o100644, file_hash),
    );

    let subtree_hash = store.write_tree(&subtree).unwrap();

    // Create root tree with subdirectory
    let mut root_tree = Tree::new();
    root_tree.insert(
        Path::new("subdir"),
        Entry::tree(subtree_hash),
    );

    let _root_hash = store.write_tree(&root_tree).unwrap();
}

/// Test Git object directory structure
#[test]
fn test_object_directory_structure() {
    let temp_dir = TempDir::new().unwrap();
    let blob_store = BlobStore::new(temp_dir.path().to_path_buf());

    let content = b"test";
    let hash = git::hash_blob(content);
    blob_store.write_blob(hash, content).unwrap();

    let hex = hash.to_hex();

    // Verify directory structure: objects/ab/cdef...
    let prefix = &hex[..2];
    let suffix = &hex[2..];

    let object_path = temp_dir.path().join("objects").join(prefix).join(suffix);
    assert!(
        object_path.exists(),
        "Object should exist at objects/{}/{}",
        prefix, suffix
    );

    // Verify it's a file
    assert!(object_path.is_file());
}

/// Comprehensive integration test: Create a realistic tree and verify Git can work with it
#[test]
fn test_realistic_project_tree() {
    use std::io::Write;

    let temp_dir = TempDir::new().unwrap();

    // Initialize Git repo
    Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["init", "--quiet"])
        .status()
        .unwrap();

    let git_dir = temp_dir.path().join(".git");
    let objects_dir = git_dir.join("objects");
    let blob_store = BlobStore::new(git_dir.clone());

    // Helper to write tree to Git objects directory
    let write_tree_to_git = |tree: &Tree| -> Sha1Hash {
        let serialized = tree.serialize();
        let hash = tree.hash();
        let hex = hash.to_hex();
        let prefix = &hex[..2];
        let suffix = &hex[2..];
        let tree_dir = objects_dir.join(prefix);
        fs::create_dir_all(&tree_dir).unwrap();
        let tree_path = tree_dir.join(suffix);
        fs::File::create(&tree_path).unwrap().write_all(&serialized).unwrap();
        hash
    };

    // Create blobs
    let readme_content = b"# My Project\n";
    let readme_hash = git::hash_blob(readme_content);
    blob_store.write_blob(readme_hash, readme_content).unwrap();

    let main_rs_content = b"fn main() {\n    println!(\"Hello\");\n}\n";
    let main_rs_hash = git::hash_blob(main_rs_content);
    blob_store.write_blob(main_rs_hash, main_rs_content).unwrap();

    let lib_rs_content = b"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let lib_rs_hash = git::hash_blob(lib_rs_content);
    blob_store.write_blob(lib_rs_hash, lib_rs_content).unwrap();

    let script_content = b"#!/bin/bash\necho test\n";
    let script_hash = git::hash_blob(script_content);
    blob_store.write_blob(script_hash, script_content).unwrap();

    // Create src/ subtree
    let mut src_tree = Tree::new();
    src_tree.insert(Path::new("main.rs"), Entry::file(0o100644, main_rs_hash));
    src_tree.insert(Path::new("lib.rs"), Entry::file(0o100644, lib_rs_hash));
    let src_tree_hash = write_tree_to_git(&src_tree);

    // Create root tree
    let mut root_tree = Tree::new();
    root_tree.insert(Path::new("README.md"), Entry::file(0o100644, readme_hash));
    root_tree.insert(Path::new("build.sh"), Entry::executable(script_hash));
    root_tree.insert(Path::new("src"), Entry::tree(src_tree_hash));

    let root_hash = write_tree_to_git(&root_tree);
    let root_hex = root_hash.to_hex();

    // Verify Git can list the entire tree
    let output = Command::new("git")
        .current_dir(temp_dir.path())
        .args(&["ls-tree", "-r", &root_hex])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Git failed to list tree: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let listing = String::from_utf8(output.stdout).unwrap();
    assert!(listing.contains("README.md"));
    assert!(listing.contains("build.sh"));
    assert!(listing.contains("src/main.rs"));
    assert!(listing.contains("src/lib.rs"));
    assert!(listing.contains("100755"), "Executable bit should be preserved");
}

/// Test that our hash implementation exactly matches Git's for known values
#[test]
fn test_known_git_hashes() {
    // Test known Git blob hashes
    // These can be verified with: echo -n "<content>" | git hash-object --stdin

    let test_cases: Vec<(&[u8], &str)> = vec![
        (b"", "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"),
        (b"hello world", "95d09f2b10159347eece71399a7e2e907ea3df4f"),
        (b"test\n", "9daeafb9864cf43055ae93beb0afd6c7d144bfa4"),
    ];

    for (content, expected_hex) in test_cases {
        let hash = git::hash_blob(content);
        assert_eq!(
            hash.to_hex(),
            expected_hex,
            "Hash mismatch for content: {:?}",
            std::str::from_utf8(content)
        );
    }
}
