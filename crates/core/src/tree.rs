//! Tree representation for repository snapshots (Git-compatible)

use crate::hash::Sha1Hash;
use anyhow::Result;
use ahash::AHashMap;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use smallvec::SmallVec;
use std::io::{Read, Write};
use std::path::Path;

/// Type of tree entry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file
    File,
    /// Executable file
    ExecutableFile,
    /// Symbolic link
    Symlink,
    /// Subdirectory (tree)
    Tree,
}

impl EntryKind {
    /// Get Git mode for this entry kind
    pub fn git_mode(&self, custom_mode: Option<u32>) -> u32 {
        match self {
            EntryKind::File => custom_mode.unwrap_or(0o100644),
            EntryKind::ExecutableFile => 0o100755,
            EntryKind::Symlink => 0o120000,
            EntryKind::Tree => 0o040000,
        }
    }

    /// Parse entry kind from Git mode
    pub fn from_git_mode(mode: u32) -> Self {
        match mode {
            0o040000 => EntryKind::Tree,
            0o100755 => EntryKind::ExecutableFile,
            0o120000 => EntryKind::Symlink,
            _ => EntryKind::File, // Default to regular file
        }
    }
}

/// Entry in a tree (file, symlink, etc.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Kind of entry
    pub kind: EntryKind,
    /// Unix permission bits (mode)
    pub mode: u32,
    /// Hash of the blob/tree containing this entry's content
    pub blob_hash: Sha1Hash,
}

impl Entry {
    /// Create a new file entry
    /// Mode should be Unix permission bits (e.g., 0o644 or 0o755)
    pub fn file(mode: u32, blob_hash: Sha1Hash) -> Self {
        // Convert Unix permission bits to Git file mode
        let git_mode = if mode & 0o111 != 0 {
            0o100755 // Executable
        } else {
            0o100644 // Regular file
        };
        Self {
            kind: if git_mode == 0o100755 {
                EntryKind::ExecutableFile
            } else {
                EntryKind::File
            },
            mode: git_mode,
            blob_hash,
        }
    }

    /// Create a new executable file entry
    pub fn executable(blob_hash: Sha1Hash) -> Self {
        Self {
            kind: EntryKind::ExecutableFile,
            mode: 0o100755,
            blob_hash,
        }
    }

    /// Create a new symlink entry
    pub fn symlink(blob_hash: Sha1Hash) -> Self {
        Self {
            kind: EntryKind::Symlink,
            mode: 0o120000,
            blob_hash,
        }
    }

    /// Create a new tree entry (subdirectory)
    pub fn tree(tree_hash: Sha1Hash) -> Self {
        Self {
            kind: EntryKind::Tree,
            mode: 0o040000,
            blob_hash: tree_hash,
        }
    }

    /// Get the Git mode for this entry
    pub fn git_mode(&self) -> u32 {
        self.kind.git_mode(Some(self.mode))
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

    /// Serialize tree to Git tree format (uncompressed)
    ///
    /// Git tree format:
    /// - Each entry: "<mode> <name>\0<20-byte-sha1>"
    /// - Entries sorted by name
    /// - No header in the raw tree data (header added when hashing)
    pub fn serialize_git_tree(&self) -> Vec<u8> {
        let mut entries_data = Vec::new();

        // Sort entries by path name (Git requirement)
        let mut sorted_entries: Vec<_> = self.entries.iter().collect();
        sorted_entries.sort_by(|(path_a, _), (path_b, _)| path_a.cmp(path_b));

        // Write each entry in Git format: "<mode> <name>\0<20-byte-sha1>"
        for (path_bytes, entry) in sorted_entries {
            // Git mode as octal string (no leading 0o)
            let mode_str = format!("{:o}", entry.git_mode());
            entries_data.extend_from_slice(mode_str.as_bytes());
            entries_data.push(b' ');

            // Path name
            entries_data.extend_from_slice(path_bytes);
            entries_data.push(0); // Null separator

            // SHA-1 hash (20 bytes, raw binary)
            entries_data.extend_from_slice(entry.blob_hash.as_bytes());
        }

        entries_data
    }

    /// Serialize tree to Git object format (with header, compressed)
    ///
    /// Git object format: "tree <size>\0<tree-data>" (zlib compressed)
    pub fn serialize(&self) -> Vec<u8> {
        let tree_data = self.serialize_git_tree();

        // Git tree object format: "tree <size>\0<data>"
        let header = format!("tree {}\0", tree_data.len());
        let mut git_object = Vec::new();
        git_object.extend_from_slice(header.as_bytes());
        git_object.extend_from_slice(&tree_data);

        // Compress with zlib
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&git_object).unwrap();
        encoder.finish().unwrap()
    }

    /// Deserialize tree from Git object format (compressed)
    pub fn deserialize(compressed: &[u8]) -> Result<Self> {
        // Decompress with zlib
        let mut decoder = ZlibDecoder::new(compressed);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;

        // Parse Git tree object format: "tree <size>\0<data>"
        if !decompressed.starts_with(b"tree ") {
            anyhow::bail!("Invalid Git tree format: missing 'tree ' header");
        }

        // Find null byte separator
        let null_pos = decompressed
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid Git tree format: missing null separator"))?;

        // Parse size from header
        let size_str = std::str::from_utf8(&decompressed[5..null_pos])?;
        let expected_size: usize = size_str.parse()?;

        // Extract tree data after null byte
        let tree_data = &decompressed[null_pos + 1..];

        // Verify size matches
        if tree_data.len() != expected_size {
            anyhow::bail!(
                "Git tree size mismatch: header says {}, got {} bytes",
                expected_size,
                tree_data.len()
            );
        }

        // Parse Git tree entries
        Self::parse_git_tree(tree_data)
    }

    /// Parse Git tree format entries
    fn parse_git_tree(data: &[u8]) -> Result<Self> {
        let mut entries = AHashMap::new();
        let mut offset = 0;

        while offset < data.len() {
            // Parse mode (octal string)
            let space_pos = data[offset..]
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| anyhow::anyhow!("Invalid tree entry: missing space after mode"))?;

            let mode_str = std::str::from_utf8(&data[offset..offset + space_pos])?;
            let mode = u32::from_str_radix(mode_str, 8)?;
            offset += space_pos + 1;

            // Parse name (until null byte)
            let null_pos = data[offset..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| anyhow::anyhow!("Invalid tree entry: missing null after name"))?;

            let name_bytes = &data[offset..offset + null_pos];
            let path_key = SmallVec::from_slice(name_bytes);
            offset += null_pos + 1;

            // Parse SHA-1 hash (20 bytes)
            if offset + 20 > data.len() {
                anyhow::bail!("Invalid tree entry: incomplete hash");
            }

            let mut hash_bytes = [0u8; 20];
            hash_bytes.copy_from_slice(&data[offset..offset + 20]);
            let blob_hash = Sha1Hash::from_bytes(hash_bytes);
            offset += 20;

            // Create entry
            let kind = EntryKind::from_git_mode(mode);
            let entry = Entry {
                kind,
                mode,
                blob_hash,
            };

            entries.insert(path_key, entry);
        }

        Ok(Self { entries })
    }

    /// Compute the hash of this tree (Git-compatible)
    ///
    /// Hash is deterministic - same tree content always produces same hash
    pub fn hash(&self) -> Sha1Hash {
        use crate::hash::git::hash_tree;

        // Convert entries to format expected by hash_tree
        let mut entries_vec: Vec<(String, u32, Sha1Hash)> = self
            .entries
            .iter()
            .map(|(path_bytes, entry)| {
                let path_str = String::from_utf8_lossy(path_bytes).to_string();
                (path_str, entry.git_mode(), entry.blob_hash)
            })
            .collect();

        // Sort by name (Git requirement)
        entries_vec.sort_by(|(name_a, _, _), (name_b, _, _)| name_a.cmp(name_b));

        hash_tree(&entries_vec)
    }

    /// Update entries in the tree
    ///
    /// Changes: Vec<(Path, Option<Entry>)>
    /// - None = remove entry
    /// - Some(entry) = insert/update entry
    pub fn update_entries(base: &Tree, changes: Vec<(&Path, Option<Entry>)>) -> Self {
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
    use crate::hash::git::hash_blob;
    use std::path::PathBuf;

    fn create_test_entry() -> Entry {
        let data = b"test file content";
        let hash = hash_blob(data);
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
    fn test_tree_git_format() -> Result<()> {
        let mut tree = Tree::new();

        let data = b"test content";
        let hash = hash_blob(data);
        tree.insert(&PathBuf::from("test.txt"), Entry::file(0o644, hash));

        // Serialize to Git format
        let git_data = tree.serialize_git_tree();

        // Should start with mode
        assert!(git_data.starts_with(b"100644"));

        Ok(())
    }

    #[test]
    fn test_tree_serialization_roundtrip() -> Result<()> {
        let mut tree = Tree::new();

        let data1 = b"file1";
        let data2 = b"file2";
        let hash1 = hash_blob(data1);
        let hash2 = hash_blob(data2);

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
        let hash = hash_blob(data);

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
    fn test_tree_executable() -> Result<()> {
        let mut tree = Tree::new();

        let hash = hash_blob(b"#!/bin/bash");
        tree.insert(&PathBuf::from("script.sh"), Entry::executable(hash));

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        let entry = deserialized.get(&PathBuf::from("script.sh")).unwrap();
        assert_eq!(entry.kind, EntryKind::ExecutableFile);
        assert_eq!(entry.mode, 0o100755);

        Ok(())
    }

    #[test]
    fn test_tree_symlink() -> Result<()> {
        let mut tree = Tree::new();

        let hash = hash_blob(b"target");
        tree.insert(&PathBuf::from("link"), Entry::symlink(hash));

        let serialized = tree.serialize();
        let deserialized = Tree::deserialize(&serialized)?;

        let entry = deserialized.get(&PathBuf::from("link")).unwrap();
        assert_eq!(entry.kind, EntryKind::Symlink);
        assert_eq!(entry.mode, 0o120000);

        Ok(())
    }

    #[test]
    fn test_tree_hash_deterministic() {
        let mut tree1 = Tree::new();
        let mut tree2 = Tree::new();

        let hash = hash_blob(b"content");
        tree1.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash));
        tree2.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash));

        // Same tree should have same hash
        assert_eq!(tree1.hash(), tree2.hash());
    }

    #[test]
    fn test_tree_hash_order_independent() {
        let mut tree1 = Tree::new();
        let mut tree2 = Tree::new();

        let hash1 = hash_blob(b"content1");
        let hash2 = hash_blob(b"content2");

        // Insert in different order
        tree1.insert(&PathBuf::from("a.txt"), Entry::file(0o644, hash1));
        tree1.insert(&PathBuf::from("b.txt"), Entry::file(0o644, hash2));

        tree2.insert(&PathBuf::from("b.txt"), Entry::file(0o644, hash2));
        tree2.insert(&PathBuf::from("a.txt"), Entry::file(0o644, hash1));

        // Hash should be same (entries are sorted)
        assert_eq!(tree1.hash(), tree2.hash());
    }

    #[test]
    fn test_tree_diff_no_changes() {
        let mut tree = Tree::new();
        let hash = hash_blob(b"content");
        tree.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash));

        let diff = TreeDiff::diff(&tree, &tree);
        assert!(diff.is_empty());
    }

    #[test]
    fn test_tree_diff_additions() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash1 = hash_blob(b"content1");
        let hash2 = hash_blob(b"content2");

        old_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));

        new_tree.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));
        new_tree.insert(&PathBuf::from("file2.txt"), Entry::file(0o644, hash2));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.modified.len(), 0);
    }

    #[test]
    fn test_tree_diff_removals() {
        let mut old_tree = Tree::new();
        let mut new_tree = Tree::new();

        let hash1 = hash_blob(b"content1");
        let hash2 = hash_blob(b"content2");

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

        let hash1 = hash_blob(b"old content");
        let hash2 = hash_blob(b"new content");

        old_tree.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash1));
        new_tree.insert(&PathBuf::from("file.txt"), Entry::file(0o644, hash2));

        let diff = TreeDiff::diff(&old_tree, &new_tree);

        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.modified.len(), 1);
    }

    #[test]
    fn test_tree_update_entries() {
        let mut base = Tree::new();
        let hash1 = hash_blob(b"content1");
        let hash2 = hash_blob(b"content2");

        base.insert(&PathBuf::from("file1.txt"), Entry::file(0o644, hash1));

        let path2 = PathBuf::from("file2.txt");
        let changes = vec![(path2.as_path(), Some(Entry::file(0o644, hash2)))];

        let new_tree = Tree::update_entries(&base, changes);

        assert_eq!(new_tree.len(), 2);
        assert!(new_tree.get(&PathBuf::from("file1.txt")).is_some());
        assert!(new_tree.get(&PathBuf::from("file2.txt")).is_some());
    }
}
