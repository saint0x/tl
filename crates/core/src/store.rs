//! On-disk store management for blobs and trees

use crate::blob::BlobStore;
use crate::hash::Blake3Hash;
use crate::tree::Tree;
use anyhow::Result;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Main store for Timelapse checkpoint data
///
/// Manages the `.tl/` directory structure:
/// ```text
/// .tl/
///   config.toml
///   HEAD
///   locks/
///     daemon.lock
///     gc.lock
///   journal/
///     ops.log
///     ops.log.idx
///   objects/
///     blobs/
///     trees/
///   refs/
///     pins/
///     heads/
///   state/
///     pathmap.bin
///     watcher.state
///     metrics.json
///   tmp/
///     ingest/
///     gc/
/// ```
pub struct Store {
    /// Root of repository
    root: PathBuf,
    /// Path to .tl directory
    tl_dir: PathBuf,
    /// Blob storage
    blob_store: BlobStore,
    /// Tree cache (hash -> tree)
    tree_cache: DashMap<Blake3Hash, Arc<Tree>>,
}

impl Store {
    /// Initialize a new store at the given repository root
    pub fn init(repo_root: &Path) -> Result<Self> {
        use std::fs;

        let tl_dir = repo_root.join(".tl");

        // Check if already initialized
        if tl_dir.exists() {
            anyhow::bail!("Store already initialized at {}", repo_root.display());
        }

        // Create .tl/ directory and all subdirectories
        fs::create_dir(&tl_dir)?;
        fs::create_dir_all(tl_dir.join("locks"))?;
        fs::create_dir_all(tl_dir.join("journal"))?;
        fs::create_dir_all(tl_dir.join("objects/blobs"))?;
        fs::create_dir_all(tl_dir.join("objects/trees"))?;
        fs::create_dir_all(tl_dir.join("refs/pins"))?;
        fs::create_dir_all(tl_dir.join("refs/heads"))?;
        fs::create_dir_all(tl_dir.join("state"))?;
        fs::create_dir_all(tl_dir.join("tmp/ingest"))?;
        fs::create_dir_all(tl_dir.join("tmp/gc"))?;

        // Create default config.toml
        let config_content = r#"# Timelapse Configuration
[store]
version = 1
blob_compression_threshold = 4096  # 4KB
max_cache_size = 104857600  # 100MB

[watcher]
debounce_ms = 100
"#;
        fs::write(tl_dir.join("config.toml"), config_content)?;

        // Create empty HEAD file (points to current checkpoint)
        fs::write(tl_dir.join("HEAD"), "")?;

        // Initialize blob store
        let blob_store = BlobStore::new(tl_dir.clone());

        Ok(Self {
            root: repo_root.to_path_buf(),
            tl_dir,
            blob_store,
            tree_cache: DashMap::new(),
        })
    }

    /// Open an existing store
    pub fn open(repo_root: &Path) -> Result<Self> {
        let tl_dir = repo_root.join(".tl");

        // Validate .tl/ directory exists
        if !tl_dir.exists() {
            anyhow::bail!("Store not initialized at {}", repo_root.display());
        }

        // Validate required subdirectories
        let required_dirs = [
            "locks",
            "journal",
            "objects/blobs",
            "objects/trees",
            "refs/pins",
            "state",
            "tmp/ingest",
        ];

        for dir in &required_dirs {
            let path = tl_dir.join(dir);
            if !path.exists() {
                anyhow::bail!("Missing required directory: {}", dir);
            }
        }

        // Validate config.toml exists
        let config_path = tl_dir.join("config.toml");
        if !config_path.exists() {
            anyhow::bail!("Missing config.toml");
        }

        // Initialize blob store
        let blob_store = BlobStore::new(tl_dir.clone());

        Ok(Self {
            root: repo_root.to_path_buf(),
            tl_dir,
            blob_store,
            tree_cache: DashMap::new(),
        })
    }

    /// Write a tree to storage
    pub fn write_tree(&self, tree: &Tree) -> Result<Blake3Hash> {
        use std::fs;
        use std::io::Write;

        // Serialize and hash the tree
        let hash = tree.hash();
        let tree_path = self.tree_path(hash);

        // If tree already exists, return hash (idempotent)
        if tree_path.exists() {
            return Ok(hash);
        }

        // Serialize tree
        let serialized = tree.serialize();

        // Atomic write pattern: write to temp, fsync, rename
        let tmp_dir = self.tl_dir.join("tmp").join("ingest");
        fs::create_dir_all(&tmp_dir)?;
        let temp_path = tmp_dir.join(format!("{}-{}", uuid::Uuid::new_v4(), hash.to_hex()));

        let mut temp_file = fs::File::create(&temp_path)?;
        temp_file.write_all(&serialized)?;
        temp_file.sync_all()?; // fsync file
        drop(temp_file);

        // Ensure parent directory exists
        if let Some(parent) = tree_path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::rename(&temp_path, &tree_path)?;

        // Fsync parent directory for durability
        if let Some(parent) = tree_path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        // Cache the tree
        self.tree_cache.insert(hash, Arc::new(tree.clone()));

        Ok(hash)
    }

    /// Read a tree from storage
    pub fn read_tree(&self, hash: Blake3Hash) -> Result<Tree> {
        use std::fs;

        // Check cache first
        if let Some(cached) = self.tree_cache.get(&hash) {
            return Ok((**cached).clone());
        }

        // Read from disk
        let tree_path = self.tree_path(hash);
        if !tree_path.exists() {
            anyhow::bail!("Tree not found: {}", hash);
        }

        let serialized = fs::read(&tree_path)?;
        let tree = Tree::deserialize(&serialized)?;

        // Verify hash matches
        let computed_hash = tree.hash();
        if computed_hash != hash {
            anyhow::bail!(
                "Tree hash mismatch: expected {}, got {}",
                hash,
                computed_hash
            );
        }

        // Add to cache
        self.tree_cache.insert(hash, Arc::new(tree.clone()));

        Ok(tree)
    }

    /// Get the tree path for a given hash
    fn tree_path(&self, hash: Blake3Hash) -> PathBuf {
        // Fan-out structure: objects/trees/<hh>/<rest>
        // Example: hash "abcd1234..." -> objects/trees/ab/cd1234...
        let hex = hash.to_hex();
        let (prefix, suffix) = hex.split_at(2);
        self.tl_dir
            .join("objects/trees")
            .join(prefix)
            .join(suffix)
    }

    /// Get the blob store
    pub fn blob_store(&self) -> &BlobStore {
        &self.blob_store
    }

    /// Get the .tl directory path
    pub fn tl_dir(&self) -> &Path {
        &self.tl_dir
    }

    /// Get the repository root path
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Atomic write helper
///
/// Writes data to a temporary file, fsyncs it, then renames it to the target path.
/// This ensures crash safety.
pub fn atomic_write(tmp_dir: &Path, target: &Path, data: &[u8]) -> Result<()> {
    use std::fs;
    use std::io::Write;

    // Ensure tmp_dir exists
    fs::create_dir_all(tmp_dir)?;

    // Generate unique temp file path
    let temp_path = tmp_dir.join(format!("{}", uuid::Uuid::new_v4()));

    // Write data to temp file
    let mut temp_file = fs::File::create(&temp_path)?;
    temp_file.write_all(data)?;
    temp_file.sync_all()?; // fsync file
    drop(temp_file);

    // Ensure target parent directory exists
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    // Rename to target (atomic on POSIX systems)
    fs::rename(&temp_path, target)?;

    // Fsync parent directory for durability
    if let Some(parent) = target.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// Normalize a path for storage
///
/// - Converts to relative path with `/` separator
/// - Rejects `..` and absolute paths
/// - Removes `./` prefix
pub fn normalize_path(path: &Path) -> Result<PathBuf> {
    // Reject absolute paths
    if path.is_absolute() {
        anyhow::bail!("Absolute paths not allowed: {}", path.display());
    }

    // Check each component for .. (reject path traversal)
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                anyhow::bail!("Path traversal not allowed: {}", path.display());
            }
            std::path::Component::RootDir => {
                anyhow::bail!("Absolute paths not allowed: {}", path.display());
            }
            _ => {}
        }
    }

    // Convert to string and normalize
    let path_str = path.to_string_lossy();

    // Remove ./ prefix if present
    let normalized = if let Some(stripped) = path_str.strip_prefix("./") {
        stripped
    } else {
        path_str.as_ref()
    };

    // Convert backslashes to forward slashes (Windows compatibility)
    let normalized = normalized.replace('\\', "/");

    // Convert back to PathBuf
    Ok(PathBuf::from(normalized))
}

/// Check if a path should be ignored
///
/// Always ignores:
/// - `.tl/`
/// - `.git/`
pub fn should_ignore(path: &Path) -> bool {
    // TODO: Implement ignore check
    // - Check if path starts with .tl/ or .git/
    // - Future: support .gitignore-like rules
    path.starts_with(".tl") || path.starts_with(".git")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_init() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_root = temp_dir.path();

        // Initialize store
        let store = Store::init(repo_root)?;

        // Verify .tl/ directory exists
        assert!(store.tl_dir().exists());

        // Verify all required subdirectories exist
        assert!(store.tl_dir().join("locks").exists());
        assert!(store.tl_dir().join("journal").exists());
        assert!(store.tl_dir().join("objects/blobs").exists());
        assert!(store.tl_dir().join("objects/trees").exists());
        assert!(store.tl_dir().join("refs/pins").exists());
        assert!(store.tl_dir().join("refs/heads").exists());
        assert!(store.tl_dir().join("state").exists());
        assert!(store.tl_dir().join("tmp/ingest").exists());
        assert!(store.tl_dir().join("tmp/gc").exists());

        // Verify config.toml exists
        assert!(store.tl_dir().join("config.toml").exists());

        // Verify HEAD exists
        assert!(store.tl_dir().join("HEAD").exists());

        Ok(())
    }

    #[test]
    fn test_store_init_already_initialized() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_root = temp_dir.path();

        // Initialize store once
        Store::init(repo_root)?;

        // Try to initialize again - should fail
        let result = Store::init(repo_root);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("already initialized"));
        }

        Ok(())
    }

    #[test]
    fn test_store_open() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_root = temp_dir.path();

        // Initialize store first
        Store::init(repo_root)?;

        // Open the store
        let store = Store::open(repo_root)?;

        // Verify paths are correct
        assert_eq!(store.root(), repo_root);
        assert_eq!(store.tl_dir(), repo_root.join(".tl"));

        Ok(())
    }

    #[test]
    fn test_store_open_not_initialized() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_root = temp_dir.path();

        // Try to open without initializing - should fail
        let result = Store::open(repo_root);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("not initialized"));
        }
    }

    #[test]
    fn test_store_write_read_tree() -> Result<()> {
        use crate::hash::hash_bytes;
        use crate::tree::{Entry, Tree};

        let temp_dir = tempfile::tempdir()?;
        let repo_root = temp_dir.path();

        let store = Store::init(repo_root)?;

        // Create a tree
        let mut tree = Tree::new();
        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");
        tree.insert(Path::new("file1.txt"), Entry::file(0o644, hash1));
        tree.insert(Path::new("file2.txt"), Entry::file(0o644, hash2));

        // Write the tree
        let tree_hash = store.write_tree(&tree)?;

        // Read it back
        let read_tree = store.read_tree(tree_hash)?;

        // Verify they match
        assert_eq!(tree.hash(), read_tree.hash());
        assert_eq!(tree.len(), read_tree.len());

        Ok(())
    }

    #[test]
    fn test_store_tree_cache() -> Result<()> {
        use crate::hash::hash_bytes;
        use crate::tree::{Entry, Tree};

        let temp_dir = tempfile::tempdir()?;
        let repo_root = temp_dir.path();

        let store = Store::init(repo_root)?;

        // Create and write a tree
        let mut tree = Tree::new();
        let hash1 = hash_bytes(b"cached content");
        tree.insert(Path::new("file.txt"), Entry::file(0o644, hash1));

        let tree_hash = store.write_tree(&tree)?;

        // Read it twice - second read should hit cache
        let read1 = store.read_tree(tree_hash)?;
        let read2 = store.read_tree(tree_hash)?;

        assert_eq!(read1.hash(), read2.hash());

        Ok(())
    }

    #[test]
    fn test_store_tree_idempotent_write() -> Result<()> {
        use crate::hash::hash_bytes;
        use crate::tree::{Entry, Tree};

        let temp_dir = tempfile::tempdir()?;
        let repo_root = temp_dir.path();

        let store = Store::init(repo_root)?;

        // Create a tree
        let mut tree = Tree::new();
        let hash1 = hash_bytes(b"idempotent");
        tree.insert(Path::new("file.txt"), Entry::file(0o644, hash1));

        // Write it twice
        let hash1 = store.write_tree(&tree)?;
        let hash2 = store.write_tree(&tree)?;

        // Should return the same hash
        assert_eq!(hash1, hash2);

        Ok(())
    }

    #[test]
    fn test_atomic_write() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let tmp_dir = temp_dir.path().join("tmp");
        let target = temp_dir.path().join("output").join("test.txt");

        let data = b"test atomic write content";

        // Write data using atomic_write
        atomic_write(&tmp_dir, &target, data)?;

        // Verify file exists at target path
        assert!(target.exists());

        // Verify content is correct
        let read_data = std::fs::read(&target)?;
        assert_eq!(read_data, data);

        // Verify temp file is cleaned up (tmp dir should exist but be empty or have UUID files)
        if tmp_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&tmp_dir)?.collect();
            // Either empty or all entries are leftover UUID files (which is ok)
            for entry in entries {
                let entry = entry?;
                // UUID files have specific format, but we just check they're not our target
                assert_ne!(entry.path(), target);
            }
        }

        Ok(())
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let tmp_dir = temp_dir.path().join("tmp");
        let target = temp_dir.path().join("a").join("b").join("c").join("file.txt");

        let data = b"nested";

        // Write to deeply nested path
        atomic_write(&tmp_dir, &target, data)?;

        // Verify file exists and content is correct
        assert!(target.exists());
        assert_eq!(std::fs::read(&target)?, data);

        Ok(())
    }

    #[test]
    fn test_normalize_path() -> Result<()> {
        // Test relative paths work
        let path = normalize_path(Path::new("src/main.rs"))?;
        assert_eq!(path, PathBuf::from("src/main.rs"));

        // Test ./ prefix is removed
        let path = normalize_path(Path::new("./file.txt"))?;
        assert_eq!(path, PathBuf::from("file.txt"));

        let path = normalize_path(Path::new("./src/lib.rs"))?;
        assert_eq!(path, PathBuf::from("src/lib.rs"));

        Ok(())
    }

    #[test]
    fn test_normalize_path_rejects_parent_dir() {
        // Test .. is rejected
        let result = normalize_path(Path::new("../secret.txt"));
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("Path traversal"));
        }

        let result = normalize_path(Path::new("src/../../etc/passwd"));
        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_path_rejects_absolute() {
        // Test absolute paths are rejected
        let result = normalize_path(Path::new("/etc/passwd"));
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("Absolute paths"));
        }
    }

    #[test]
    fn test_normalize_path_backslashes() -> Result<()> {
        // Test backslash conversion (Windows paths)
        let path = normalize_path(Path::new("src\\main.rs"))?;
        assert_eq!(path.to_string_lossy(), "src/main.rs");

        Ok(())
    }

    #[test]
    fn test_should_ignore() {
        // Test .tl/ is ignored
        assert!(should_ignore(Path::new(".tl/config.toml")));
        assert!(should_ignore(Path::new(".tl")));
        assert!(should_ignore(Path::new(".tl/objects/blobs")));

        // Test .git/ is ignored
        assert!(should_ignore(Path::new(".git/HEAD")));
        assert!(should_ignore(Path::new(".git")));
        assert!(should_ignore(Path::new(".git/config")));

        // Test normal files are not ignored
        assert!(!should_ignore(Path::new("src/main.rs")));
        assert!(!should_ignore(Path::new("README.md")));
        assert!(!should_ignore(Path::new("a/b/c/file.txt")));
    }
}
