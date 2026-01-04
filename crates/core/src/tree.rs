//! Tree representation for repository snapshots

use crate::hash::Blake3Hash;
use anyhow::Result;
use ahash::AHashMap;
use smallvec::SmallVec;
use std::path::Path;

/// Type of tree entry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file
    File,
    /// Symbolic link
    Symlink,
    /// Submodule (optional for MVP)
    Submodule,
}

/// Entry in a tree (file, symlink, etc.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Kind of entry
    pub kind: EntryKind,
    /// Unix permission bits (mode)
    pub mode: u32,
    /// Hash of the blob containing this entry's content
    pub blob_hash: Blake3Hash,
}

impl Entry {
    /// Create a new file entry
    pub fn file(mode: u32, blob_hash: Blake3Hash) -> Self {
        Self {
            kind: EntryKind::File,
            mode,
            blob_hash,
        }
    }

    /// Create a new symlink entry
    pub fn symlink(blob_hash: Blake3Hash) -> Self {
        Self {
            kind: EntryKind::Symlink,
            mode: 0o120000, // Standard symlink mode
            blob_hash,
        }
    }
}

/// A tree represents the complete repository state at a point in time
///
/// Uses SmallVec for paths to optimize stack allocation for short paths (< 64 bytes)
#[derive(Debug, Clone)]
pub struct Tree {
    /// Mapping from path to entry
    /// Uses AHashMap (faster for small keys) and SmallVec (stack allocation for short paths)
    entries: AHashMap<SmallVec<[u8; 64]>, Entry>,
}

/// Convert a Path to SmallVec<[u8; 64]> for use as tree key
fn path_to_key(path: &Path) -> SmallVec<[u8; 64]> {
    let path_str = path.to_string_lossy();
    let bytes = path_str.as_bytes();
    SmallVec::from_slice(bytes)
}

impl Tree {
    /// Create a new empty tree
    pub fn new() -> Self {
        Self {
            entries: AHashMap::new(),
        }
    }

    /// Insert an entry into the tree
    pub fn insert(&mut self, path: &Path, entry: Entry) {
        let key = path_to_key(path);
        self.entries.insert(key, entry);
    }

    /// Get an entry from the tree
    pub fn get(&self, path: &Path) -> Option<&Entry> {
        let key = path_to_key(path);
        self.entries.get(&key)
    }

    /// Remove an entry from the tree
    pub fn remove(&mut self, path: &Path) -> Option<Entry> {
        let key = path_to_key(path);
        self.entries.remove(&key)
    }

    /// Get the number of entries in the tree
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the tree is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get an iterator over all entries
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// Get an iterator over all entries with their paths (as byte slices)
    pub fn entries_with_paths(&self) -> impl Iterator<Item = (&[u8], &Entry)> {
        self.entries.iter().map(|(k, v)| (k.as_slice(), v))
    }

    /// Serialize the tree to bytes (TreeV1 format)
    ///
    /// Format:
    /// - magic: "SNT1" (4 bytes)
    /// - entry_count: u32
    /// - entries (sorted lexicographically by path):
    ///   - path_len: u16
    ///   - path_bytes: [u8; path_len]
    ///   - kind: u8 (0=file, 1=symlink, 2=submodule)
    ///   - mode: u32
    ///   - blob_hash: [u8; 32]
    pub fn serialize(&self) -> Vec<u8> {
        const MAGIC: &[u8] = b"SNT1";

        let mut bytes = Vec::new();

        // Write magic
        bytes.extend_from_slice(MAGIC);

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

        bytes
    }

    /// Deserialize a tree from bytes (TreeV1 format)
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        const MAGIC: &[u8] = b"SNT1";

        if bytes.len() < 8 {
            anyhow::bail!("Invalid tree data: too short");
        }

        // Check magic
        if &bytes[0..4] != MAGIC {
            anyhow::bail!("Invalid tree magic bytes");
        }

        // Read entry count
        let entry_count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

        let mut entries = AHashMap::new();
        let mut offset = 8;

        // Parse each entry
        for _ in 0..entry_count {
            if offset + 2 > bytes.len() {
                anyhow::bail!("Invalid tree data: incomplete entry");
            }

            // Read path length
            let path_len = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) as usize;
            offset += 2;

            if offset + path_len > bytes.len() {
                anyhow::bail!("Invalid tree data: path too long");
            }

            // Read path bytes
            let path_bytes = SmallVec::from_slice(&bytes[offset..offset + path_len]);
            offset += path_len;

            if offset + 1 + 4 + 32 > bytes.len() {
                anyhow::bail!("Invalid tree data: incomplete entry metadata");
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

        Ok(Self { entries })
    }

    /// Compute the hash of this tree
    ///
    /// Hash is deterministic - same tree content always produces same hash
    pub fn hash(&self) -> Blake3Hash {
        use crate::hash::hash_bytes;
        let serialized = self.serialize();
        hash_bytes(&serialized)
    }

    /// Update entries in the tree
    ///
    /// Changes: Vec<(Path, Option<Entry>)>
    /// - None = remove entry
    /// - Some(entry) = insert/update entry
    pub fn update_entries(
        base: &Tree,
        changes: Vec<(&Path, Option<Entry>)>,
    ) -> Self {
        let mut new_tree = base.clone();

        for (path, entry_opt) in changes {
            match entry_opt {
                Some(entry) => {
                    new_tree.insert(path, entry);
                }
                None => {
                    new_tree.remove(path);
                }
            }
        }

        new_tree
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

/// Differences between two trees
#[derive(Debug, Clone)]
pub struct TreeDiff {
    /// Entries added in new tree
    pub added: Vec<(SmallVec<[u8; 64]>, Entry)>,
    /// Entries removed in new tree
    pub removed: Vec<(SmallVec<[u8; 64]>, Entry)>,
    /// Entries modified in new tree (old, new)
    pub modified: Vec<(SmallVec<[u8; 64]>, Entry, Entry)>,
}

impl TreeDiff {
    /// Compute the diff between two trees
    pub fn diff(old: &Tree, new: &Tree) -> Self {
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut modified = Vec::new();

        // Find additions and modifications
        for (path, new_entry) in &new.entries {
            match old.entries.get(path) {
                Some(old_entry) => {
                    // Entry exists in both trees - check if modified
                    if old_entry != new_entry {
                        modified.push((path.clone(), old_entry.clone(), new_entry.clone()));
                    }
                }
                None => {
                    // Entry only in new tree - addition
                    added.push((path.clone(), new_entry.clone()));
                }
            }
        }

        // Find removals
        for (path, old_entry) in &old.entries {
            if !new.entries.contains_key(path) {
                removed.push((path.clone(), old_entry.clone()));
            }
        }

        Self {
            added,
            removed,
            modified,
        }
    }

    /// Check if there are any changes
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;
    use std::path::PathBuf;

    fn create_test_entry() -> Entry {
        let data = b"test file content";
        let hash = hash_bytes(data);
        Entry::file(0o644, hash)
    }

    #[test]
    fn test_tree_insert_get() {
        let mut tree = Tree::new();
        let entry = create_test_entry();

        let path = PathBuf::from("test/file.txt");
        tree.insert(&path, entry.clone());

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(&path), Some(&entry));
    }

    #[test]
    fn test_tree_remove() {
        let mut tree = Tree::new();
        let entry = create_test_entry();

        let path = PathBuf::from("test/file.txt");
        tree.insert(&path, entry.clone());
        assert_eq!(tree.len(), 1);

        let removed = tree.remove(&path);
        assert_eq!(removed, Some(entry));
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.get(&path), None);
    }

    #[test]
    fn test_tree_serialization_roundtrip() -> Result<()> {
        let mut tree = Tree::new();

        let data1 = b"file1";
        let data2 = b"file2";
        let hash1 = hash_bytes(data1);
        let hash2 = hash_bytes(data2);

        tree.insert(&PathBuf::from("src/main.rs"), Entry::file(0o644, hash1));
        tree.insert(&PathBuf::from("README.md"), Entry::file(0o644, hash2));

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        assert_eq!(tree.len(), deserialized.len());
        assert_eq!(
            tree.get(&PathBuf::from("src/main.rs")),
            deserialized.get(&PathBuf::from("src/main.rs"))
        );
        assert_eq!(
            tree.get(&PathBuf::from("README.md")),
            deserialized.get(&PathBuf::from("README.md"))
        );

        Ok(())
    }

    #[test]
    fn test_tree_serialization_deterministic() {
        let mut tree1 = Tree::new();
        let mut tree2 = Tree::new();

        let data = b"test";
        let hash = hash_bytes(data);

        // Insert in different order
        tree1.insert(&PathBuf::from("a.txt"), Entry::file(0o644, hash));
        tree1.insert(&PathBuf::from("b.txt"), Entry::file(0o644, hash));

        tree2.insert(&PathBuf::from("b.txt"), Entry::file(0o644, hash));
        tree2.insert(&PathBuf::from("a.txt"), Entry::file(0o644, hash));

        // Serialization should be identical (sorted)
        let bytes1 = tree1.serialize();
        let bytes2 = tree2.serialize();

        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn test_tree_empty() -> Result<()> {
        let tree = Tree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        assert!(deserialized.is_empty());
        assert_eq!(deserialized.len(), 0);

        Ok(())
    }

    #[test]
    fn test_tree_magic_validation() {
        let bad_magic = b"BAD1";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(bad_magic);
        bytes.extend_from_slice(&0u32.to_le_bytes());

        assert!(Tree::deserialize(&bytes).is_err());
    }

    #[test]
    fn test_tree_multiple_entries() -> Result<()> {
        let mut tree = Tree::new();

        let paths = vec![
            "src/main.rs",
            "src/lib.rs",
            "tests/test.rs",
            "Cargo.toml",
            "README.md",
        ];

        for path in &paths {
            let data = path.as_bytes();
            let hash = hash_bytes(data);
            tree.insert(&PathBuf::from(path), Entry::file(0o644, hash));
        }

        assert_eq!(tree.len(), paths.len());

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        assert_eq!(tree.len(), deserialized.len());

        for path in &paths {
            let path_buf = PathBuf::from(path);
            assert_eq!(tree.get(&path_buf), deserialized.get(&path_buf));
        }

        Ok(())
    }

    #[test]
    fn test_tree_symlink() -> Result<()> {
        let mut tree = Tree::new();

        let hash = hash_bytes(b"target");
        tree.insert(&PathBuf::from("link"), Entry::symlink(hash));

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        let entry = deserialized.get(&PathBuf::from("link")).unwrap();
        assert_eq!(entry.kind, EntryKind::Symlink);
        assert_eq!(entry.mode, 0o120000);

        Ok(())
    }

    #[test]
    fn test_tree_different_modes() -> Result<()> {
        let mut tree = Tree::new();

        let hash = hash_bytes(b"content");
        tree.insert(&PathBuf::from("regular"), Entry::file(0o644, hash));
        tree.insert(&PathBuf::from("executable"), Entry::file(0o755, hash));

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        assert_eq!(
            deserialized.get(&PathBuf::from("regular")).unwrap().mode,
            0o644
        );
        assert_eq!(
            deserialized.get(&PathBuf::from("executable")).unwrap().mode,
            0o755
        );

        Ok(())
    }

    #[test]
    fn test_tree_long_paths() -> Result<()> {
        let mut tree = Tree::new();

        // Path longer than SmallVec inline capacity (64 bytes)
        let long_path = "a/".repeat(50) + "file.txt";
        let hash = hash_bytes(b"content");

        tree.insert(&PathBuf::from(&long_path), Entry::file(0o644, hash));

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        assert_eq!(
            tree.get(&PathBuf::from(&long_path)),
            deserialized.get(&PathBuf::from(&long_path))
        );

        Ok(())
    }

    #[test]
    fn test_tree_hash_deterministic() {
        let mut tree1 = Tree::new();
        let mut tree2 = Tree::new();

        let hash = hash_bytes(b"content");
        tree1.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash));
        tree2.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash));

        // Same tree should serialize to same bytes
        let bytes1 = tree1.serialize();
        let bytes2 = tree2.serialize();

        assert_eq!(bytes1, bytes2);
    }

    // Phase 1.5: Hashing and diffing tests

    #[test]
    fn test_tree_hash() {
        let mut tree = Tree::new();
        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        tree.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));

        let tree_hash = tree.hash();

        // Hash should be deterministic
        let tree_hash2 = tree.hash();
        assert_eq!(tree_hash, tree_hash2);
    }

    #[test]
    fn test_tree_hash_different_content() {
        let mut tree1 = Tree::new();
        let mut tree2 = Tree::new();

        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        tree1.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash1));
        tree2.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash2));

        assert_ne!(tree1.hash(), tree2.hash());
    }

    #[test]
    fn test_tree_hash_order_independent() {
        let mut tree1 = Tree::new();
        let mut tree2 = Tree::new();

        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        // Insert in different order
        tree1.insert(&PathBuf::from("a.txt"), Entry::file(0o644, hash1));
        tree1.insert(&PathBuf::from("b.txt"), Entry::file(0o644, hash2));

        tree2.insert(&PathBuf::from("b.txt"), Entry::file(0o644, hash2));
        tree2.insert(&PathBuf::from("a.txt"), Entry::file(0o644, hash1));

        // Hash should be same (serialization is sorted)
        assert_eq!(tree1.hash(), tree2.hash());
    }

    #[test]
    fn test_tree_diff_no_changes() {
        let mut tree = Tree::new();
        let hash = hash_bytes(b"content");
        tree.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash));

        let diff = TreeDiff::diff(&tree, &tree);
        assert!(diff.is_empty());
        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.modified.len(), 0);
    }

    #[test]
    fn test_tree_diff_additions() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        old_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));

        new_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        new_tree.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.modified.len(), 0);
        assert!(!diff.is_empty());
    }

    #[test]
    fn test_tree_diff_removals() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        old_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        old_tree.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));

        new_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.modified.len(), 0);
    }

    #[test]
    fn test_tree_diff_modifications() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash1 = hash_bytes(b"old content");
        let hash2 = hash_bytes(b"new content");

        old_tree.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash1));
        new_tree.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash2));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.modified.len(), 1);
    }

    #[test]
    fn test_tree_diff_mode_change() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash = hash_bytes(b"content");

        old_tree.insert(&PathBuf::from("script.sh"), Entry::file(0o644, hash));
        new_tree.insert(&PathBuf::from("script.sh"), Entry::file(0o755, hash));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        // Mode change is a modification
        assert_eq!(diff.modified.len(), 1);
    }

    #[test]
    fn test_tree_diff_complex() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");
        let hash3 = hash_bytes(b"content3");
        let hash4 = hash_bytes(b"modified");

        // Old tree: file1 (unchanged), file2 (removed), file3 (modified)
        old_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        old_tree.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));
        old_tree.insert(&PathBuf::from("file3.txt"), Entry::file(0o644, hash3));

        // New tree: file1 (unchanged), file3 (modified), file4 (added)
        new_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        new_tree.insert(&PathBuf::from("file3.txt"), Entry::file(0o644, hash4));
        new_tree.insert(&PathBuf::from("file4.txt"), Entry::file(0o644, hash2));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        assert_eq!(diff.added.len(), 1); // file4
        assert_eq!(diff.removed.len(), 1); // file2
        assert_eq!(diff.modified.len(), 1); // file3
    }

    #[test]
    fn test_tree_update_entries_additions() {
        let mut base = Tree::new();
        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        base.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));

        let path2 = PathBuf::from("file2.txt");
        let changes = vec![
            (path2.as_path(), Some(Entry::file(0o644, hash2))),
        ];

        let new_tree = Tree::update_entries(&base, changes);

        assert_eq!(new_tree.len(), 2);
        assert!(new_tree.get(&PathBuf::from("file1.txt")).is_some());
        assert!(new_tree.get(&PathBuf::from("file2.txt")).is_some());
    }

    #[test]
    fn test_tree_update_entries_removals() {
        let mut base = Tree::new();
        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");

        base.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        base.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));

        let path2 = PathBuf::from("file2.txt");
        let changes = vec![
            (path2.as_path(), None),
        ];

        let new_tree = Tree::update_entries(&base, changes);

        assert_eq!(new_tree.len(), 1);
        assert!(new_tree.get(&PathBuf::from("file1.txt")).is_some());
        assert!(new_tree.get(&PathBuf::from("file2.txt")).is_none());
    }

    #[test]
    fn test_tree_update_entries_modifications() {
        let mut base = Tree::new();
        let hash1 = hash_bytes(b"old content");
        let hash2 = hash_bytes(b"new content");

        base.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash1));

        let path = PathBuf::from("file.txt");
        let changes = vec![
            (path.as_path(), Some(Entry::file(0o644, hash2))),
        ];

        let new_tree = Tree::update_entries(&base, changes);

        assert_eq!(new_tree.len(), 1);
        let entry = new_tree.get(&PathBuf::from("file.txt")).unwrap();
        assert_eq!(entry.blob_hash, hash2);
    }

    #[test]
    fn test_tree_update_entries_mixed() {
        let mut base = Tree::new();
        let hash1 = hash_bytes(b"content1");
        let hash2 = hash_bytes(b"content2");
        let hash3 = hash_bytes(b"modified");

        base.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        base.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));

        let path1 = PathBuf::from("file1.txt");
        let path2 = PathBuf::from("file2.txt");
        let path3 = PathBuf::from("file3.txt");
        let changes = vec![
            // Modify file1
            (path1.as_path(), Some(Entry::file(0o644, hash3))),
            // Remove file2
            (path2.as_path(), None),
            // Add file3
            (path3.as_path(), Some(Entry::file(0o755, hash1))),
        ];

        let new_tree = Tree::update_entries(&base, changes);

        assert_eq!(new_tree.len(), 2);
        assert_eq!(new_tree.get(&PathBuf::from("file1.txt")).unwrap().blob_hash, hash3);
        assert!(new_tree.get(&PathBuf::from("file2.txt")).is_none());
        assert_eq!(new_tree.get(&PathBuf::from("file3.txt")).unwrap().blob_hash, hash1);
    }
}
