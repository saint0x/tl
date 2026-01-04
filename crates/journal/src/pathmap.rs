//! PathMap state cache for fast tree updates

use ahash::AHashMap;
use anyhow::Result;
use core::{Blake3Hash, Entry, EntryKind, Tree};
use smallvec::SmallVec;
use std::fs;
use std::path::Path;

/// Cached mapping of paths to entries (performance optimization)
///
/// Uses the same internal structure as Tree for consistency
#[derive(Debug, Clone)]
pub struct PathMap {
    /// Root tree hash this map corresponds to
    pub root_tree: Blake3Hash,
    /// Mapping from path to entry
    /// Uses AHashMap (faster for small keys) and SmallVec (stack allocation for short paths)
    entries: AHashMap<SmallVec<[u8; 64]>, Entry>,
}

/// Convert a Path to SmallVec<[u8; 64]> for use as map key
fn path_to_key(path: &Path) -> SmallVec<[u8; 64]> {
    let path_str = path.to_string_lossy();
    let bytes = path_str.as_bytes();
    SmallVec::from_slice(bytes)
}

impl PathMap {
    /// Create a new empty PathMap
    pub fn new(root_tree: Blake3Hash) -> Self {
        Self {
            root_tree,
            entries: AHashMap::new(),
        }
    }

    /// Update an entry in the map (None = remove)
    pub fn update(&mut self, path: &Path, entry: Option<Entry>) {
        let key = path_to_key(path);
        match entry {
            Some(e) => {
                self.entries.insert(key, e);
            }
            None => {
                self.entries.remove(&key);
            }
        }
    }

    /// Get an entry from the map
    pub fn get(&self, path: &Path) -> Option<&Entry> {
        let key = path_to_key(path);
        self.entries.get(&key)
    }

    /// Get an entry from the map by raw bytes (for internal use)
    pub fn get_by_bytes(&self, path_bytes: &[u8]) -> Option<&Entry> {
        let key = SmallVec::from_slice(path_bytes);
        self.entries.get(&key)
    }

    /// Get the number of entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the map is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries as an iterator
    pub fn entries(&self) -> impl Iterator<Item = (SmallVec<[u8; 64]>, &Entry)> {
        self.entries.iter().map(|(k, v)| (k.clone(), v))
    }

    /// Build PathMap from Tree (used when pathmap.bin is missing/corrupt)
    pub fn from_tree(tree: &Tree, root_tree: Blake3Hash) -> Self {
        let mut pathmap = Self::new(root_tree);

        // Populate from tree's entries
        for (path_bytes, entry) in tree.entries_with_paths() {
            let key = SmallVec::from_slice(path_bytes);
            pathmap.entries.insert(key, entry.clone());
        }

        pathmap
    }

    /// Save PathMap to disk (PMV1 format)
    ///
    /// Format:
    /// - magic: "PMV1" (4 bytes)
    /// - root_tree: Blake3Hash (32 bytes)
    /// - entry_count: u32 (4 bytes)
    /// - entries (sorted by path):
    ///   - path_len: u16
    ///   - path_bytes: [u8; path_len]
    ///   - kind: u8 (0=file, 1=symlink, 2=submodule)
    ///   - mode: u32
    ///   - blob_hash: [u8; 32]
    pub fn save(&self, path: &Path) -> Result<()> {
        const MAGIC: &[u8] = b"PMV1";

        let mut bytes = Vec::new();

        // Write magic
        bytes.extend_from_slice(MAGIC);

        // Write root_tree hash
        bytes.extend_from_slice(self.root_tree.as_bytes());

        // Write entry count
        bytes.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

        // Sort entries by path for deterministic serialization
        let mut sorted_entries: Vec<_> = self.entries.iter().collect();
        sorted_entries.sort_by(|(path_a, _), (path_b, _)| path_a.cmp(path_b));

        // Write each entry
        for (path_bytes, entry) in sorted_entries {
            // Path length (u16)
            let path_len = path_bytes.len() as u16;
            bytes.extend_from_slice(&path_len.to_le_bytes());

            // Path bytes
            bytes.extend_from_slice(path_bytes);

            // Kind (u8)
            let kind_byte = match entry.kind {
                EntryKind::File => 0u8,
                EntryKind::Symlink => 1u8,
                EntryKind::Submodule => 2u8,
            };
            bytes.push(kind_byte);

            // Mode (u32)
            bytes.extend_from_slice(&entry.mode.to_le_bytes());

            // Blob hash (32 bytes)
            bytes.extend_from_slice(entry.blob_hash.as_bytes());
        }

        // Atomic write using core::store::atomic_write pattern
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("PathMap path must have parent directory"))?;
        let tmp_dir = parent
            .parent()
            .ok_or_else(|| anyhow::anyhow!("PathMap path must have grandparent"))?
            .join("tmp")
            .join("ingest");

        core::store::atomic_write(&tmp_dir, path, &bytes)?;

        Ok(())
    }

    /// Load PathMap from disk (PMV1 format)
    pub fn load(path: &Path) -> Result<Self> {
        const MAGIC: &[u8] = b"PMV1";

        let bytes = fs::read(path).map_err(|e| {
            anyhow::anyhow!("Failed to read PathMap from {}: {}", path.display(), e)
        })?;

        if bytes.len() < 40 {
            anyhow::bail!(
                "Invalid PathMap: file too short (expected at least 40 bytes, got {})",
                bytes.len()
            );
        }

        // Check magic
        if &bytes[0..4] != MAGIC {
            anyhow::bail!(
                "Invalid PathMap magic: expected PMV1, got {:?}",
                &bytes[0..4]
            );
        }

        // Read root_tree hash
        let mut root_tree_bytes = [0u8; 32];
        root_tree_bytes.copy_from_slice(&bytes[4..36]);
        let root_tree = Blake3Hash::from_bytes(root_tree_bytes);

        // Read entry count
        let entry_count = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]) as usize;

        let mut entries = AHashMap::new();
        let mut offset = 40;

        // Parse each entry
        for i in 0..entry_count {
            if offset + 2 > bytes.len() {
                anyhow::bail!(
                    "Invalid PathMap: incomplete entry {} at offset {}",
                    i,
                    offset
                );
            }

            // Read path length
            let path_len = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) as usize;
            offset += 2;

            if offset + path_len > bytes.len() {
                anyhow::bail!(
                    "Invalid PathMap: path too long for entry {} at offset {}",
                    i,
                    offset
                );
            }

            // Read path bytes
            let path_bytes = SmallVec::from_slice(&bytes[offset..offset + path_len]);
            offset += path_len;

            if offset + 1 + 4 + 32 > bytes.len() {
                anyhow::bail!(
                    "Invalid PathMap: incomplete entry metadata at offset {}",
                    offset
                );
            }

            // Read kind
            let kind = match bytes[offset] {
                0 => EntryKind::File,
                1 => EntryKind::Symlink,
                2 => EntryKind::Submodule,
                _ => anyhow::bail!("Invalid entry kind: {}", bytes[offset]),
            };
            offset += 1;

            // Read mode
            let mode = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            offset += 4;

            // Read blob hash
            let mut hash_bytes = [0u8; 32];
            hash_bytes.copy_from_slice(&bytes[offset..offset + 32]);
            let blob_hash = Blake3Hash::from_bytes(hash_bytes);
            offset += 32;

            let entry = Entry {
                kind,
                mode,
                blob_hash,
            };

            entries.insert(path_bytes, entry);
        }

        Ok(Self {
            root_tree,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{Entry, EntryKind, Tree, hash::hash_bytes};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_test_entry(content: &[u8]) -> Entry {
        let hash = hash_bytes(content);
        Entry {
            kind: EntryKind::File,
            mode: 0o644,
            blob_hash: hash,
        }
    }

    #[test]
    fn test_pathmap_new() {
        let root_tree = Blake3Hash::from_bytes([1u8; 32]);
        let pathmap = PathMap::new(root_tree);

        assert_eq!(pathmap.len(), 0);
        assert!(pathmap.is_empty());
    }

    #[test]
    fn test_pathmap_update_and_get() {
        let root_tree = Blake3Hash::from_bytes([1u8; 32]);
        let mut pathmap = PathMap::new(root_tree);

        let path = PathBuf::from("test/file.txt");
        let entry = create_test_entry(b"test content");

        // Insert entry
        pathmap.update(&path, Some(entry.clone()));
        assert_eq!(pathmap.len(), 1);

        // Get entry
        let retrieved = pathmap.get_by_bytes(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(retrieved.blob_hash, entry.blob_hash);
    }

    #[test]
    fn test_pathmap_remove_entry() {
        let root_tree = Blake3Hash::from_bytes([1u8; 32]);
        let mut pathmap = PathMap::new(root_tree);

        let path = PathBuf::from("test/file.txt");
        let entry = create_test_entry(b"test content");

        // Insert then remove
        pathmap.update(&path, Some(entry));
        assert_eq!(pathmap.len(), 1);

        pathmap.update(&path, None);
        assert_eq!(pathmap.len(), 0);
        assert!(pathmap.get_by_bytes(path.to_string_lossy().as_bytes()).is_none());
    }

    #[test]
    fn test_pathmap_serialization_roundtrip() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let save_path = temp_dir.path().join("pathmap.bin");

        let root_tree = Blake3Hash::from_bytes([2u8; 32]);
        let mut pathmap = PathMap::new(root_tree);

        // Add multiple entries
        let paths = vec!["src/main.rs", "src/lib.rs", "tests/test.rs"];
        for (i, path) in paths.iter().enumerate() {
            let entry = create_test_entry(format!("content{}", i).as_bytes());
            pathmap.update(&PathBuf::from(path), Some(entry));
        }

        // Save
        pathmap.save(&save_path)?;

        // Load
        let loaded = PathMap::load(&save_path)?;

        // Verify
        assert_eq!(loaded.len(), pathmap.len());
        assert_eq!(loaded.root_tree, pathmap.root_tree);

        for path in paths {
            let path_bytes = path.as_bytes();
            assert_eq!(
                loaded.get_by_bytes(path_bytes).unwrap().blob_hash,
                pathmap.get_by_bytes(path_bytes).unwrap().blob_hash
            );
        }

        Ok(())
    }

    #[test]
    fn test_pathmap_load_missing_file() {
        let temp_dir = TempDir::new().unwrap();
        let load_path = temp_dir.path().join("nonexistent.bin");

        assert!(PathMap::load(&load_path).is_err());
    }

    #[test]
    fn test_pathmap_from_tree() {
        let mut tree = Tree::new();
        let entry1 = create_test_entry(b"content1");
        let entry2 = create_test_entry(b"content2");

        tree.insert(&PathBuf::from("file1.txt"), entry1.clone());
        tree.insert(&PathBuf::from("file2.txt"), entry2.clone());

        let tree_hash = tree.hash();
        let pathmap = PathMap::from_tree(&tree, tree_hash);

        assert_eq!(pathmap.len(), 2);
        assert_eq!(pathmap.root_tree, tree_hash);
        assert_eq!(
            pathmap.get_by_bytes(b"file1.txt").unwrap().blob_hash,
            entry1.blob_hash
        );
    }

    #[test]
    fn test_pathmap_entries_iterator() {
        let root_tree = Blake3Hash::from_bytes([3u8; 32]);
        let mut pathmap = PathMap::new(root_tree);

        // Add entries
        for i in 0..5 {
            let path = PathBuf::from(format!("file{}.txt", i));
            let entry = create_test_entry(format!("content{}", i).as_bytes());
            pathmap.update(&path, Some(entry));
        }

        // Iterate and count
        let count = pathmap.entries().count();
        assert_eq!(count, 5);
    }

    #[test]
    fn test_pathmap_deterministic_serialization() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let path1 = temp_dir.path().join("pm1.bin");
        let path2 = temp_dir.path().join("pm2.bin");

        let root_tree = Blake3Hash::from_bytes([4u8; 32]);
        let mut pathmap1 = PathMap::new(root_tree);
        let mut pathmap2 = PathMap::new(root_tree);

        // Insert in different order
        pathmap1.update(&PathBuf::from("a.txt"), Some(create_test_entry(b"a")));
        pathmap1.update(&PathBuf::from("b.txt"), Some(create_test_entry(b"b")));

        pathmap2.update(&PathBuf::from("b.txt"), Some(create_test_entry(b"b")));
        pathmap2.update(&PathBuf::from("a.txt"), Some(create_test_entry(b"a")));

        // Save both
        pathmap1.save(&path1)?;
        pathmap2.save(&path2)?;

        // Files should be identical (sorted)
        let bytes1 = std::fs::read(&path1)?;
        let bytes2 = std::fs::read(&path2)?;
        assert_eq!(bytes1, bytes2);

        Ok(())
    }
}
