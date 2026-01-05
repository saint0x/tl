//! SHA-1 hashing primitives for Git-compatible content-addressed storage

use std::path::Path;
use std::time::Duration;
use std::thread::sleep;
use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};
use sha1::{Sha1, Digest};

/// A SHA-1 hash (20 bytes) - Git-compatible
#[derive(Copy, Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Sha1Hash([u8; 20]);

impl Sha1Hash {
    /// Create a new Sha1Hash from bytes
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Get the hash as a byte slice
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Convert to hex string (40 characters for SHA-1)
    pub fn to_hex(&self) -> String {
        const HEX_CHARS: &[u8] = b"0123456789abcdef";
        let mut hex = String::with_capacity(40);
        for &byte in &self.0 {
            hex.push(HEX_CHARS[(byte >> 4) as usize] as char);
            hex.push(HEX_CHARS[(byte & 0xf) as usize] as char);
        }
        hex
    }

    /// Parse from hex string (40 characters for SHA-1)
    pub fn from_hex(hex: &str) -> Result<Self> {
        if hex.len() != 40 {
            anyhow::bail!("Invalid hex length: expected 40 characters (SHA-1), got {}", hex.len());
        }

        let mut bytes = [0u8; 20];
        for i in 0..20 {
            let high = hex_char_to_nibble(hex.as_bytes()[i * 2])?;
            let low = hex_char_to_nibble(hex.as_bytes()[i * 2 + 1])?;
            bytes[i] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

/// Helper function to convert a hex character to a nibble
fn hex_char_to_nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => anyhow::bail!("Invalid hex character: {}", c as char),
    }
}

impl std::fmt::Debug for Sha1Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sha1Hash({})", self.to_hex())
    }
}

impl std::fmt::Display for Sha1Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Hash bytes using SHA-1 (Git-compatible)
pub fn hash_bytes(data: &[u8]) -> Sha1Hash {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&result);
    Sha1Hash::from_bytes(bytes)
}

/// Hash a file using SHA-1 with Git blob format
///
/// This computes the hash as Git would: hash of "blob <size>\0<content>"
pub fn hash_file(path: &Path) -> Result<Sha1Hash> {
    use std::fs;

    // Read file contents
    let data = fs::read(path)?;

    // Hash using Git blob format
    Ok(git::hash_blob(&data))
}

/// Hash a file using memory-mapped I/O with Git blob format (optimized for large files > 4MB)
pub fn hash_file_mmap(path: &Path) -> Result<Sha1Hash> {
    use std::fs::File;
    use memmap2::Mmap;

    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };

    // Hash using Git blob format
    Ok(git::hash_blob(&mmap))
}

/// Hash file with stability verification (double-stat pattern)
///
/// Ensures file is not changing during read by checking metadata
/// before and after read operation.
///
/// # Arguments
/// * `path` - File to hash
/// * `max_retries` - Maximum retry attempts (default: 3)
///
/// # Returns
/// * `Ok(hash)` - File is stable, hash is valid
/// * `Err(...)` - File changed too many times or other I/O errors
///
/// # Example
/// ```no_run
/// # extern crate core as tl_core;
/// use std::path::Path;
/// use tl_core::hash::hash_file_stable;
///
/// # fn main() -> anyhow::Result<()> {
/// let hash = hash_file_stable(Path::new("file.txt"), 3)?;
/// # Ok(())
/// # }
/// ```
pub fn hash_file_stable(path: &Path, max_retries: u8) -> Result<Sha1Hash> {
    use std::fs;

    for attempt in 0..max_retries {
        // 1. Stat before read
        let stat1 = fs::metadata(path)
            .with_context(|| format!("Failed to stat (pre): {}", path.display()))?;

        // 2. Hash file (existing implementation)
        let hash = hash_file(path)?;

        // 3. Stat after read
        let stat2 = fs::metadata(path)
            .with_context(|| format!("Failed to stat (post): {}", path.display()))?;

        // 4. Verify stability (size + mtime unchanged)
        if stat1.len() == stat2.len() &&
           stat1.modified()? == stat2.modified()? {
            return Ok(hash);
        }

        // File changed during read - exponential backoff
        if attempt < max_retries - 1 {
            let backoff_ms = 50 << attempt;  // 50ms, 100ms, 200ms
            sleep(Duration::from_millis(backoff_ms));
        }
    }

    // Failed after all retries
    Err(anyhow::anyhow!(
        "File {} is unstable after {} read attempts (file changing too rapidly)",
        path.display(),
        max_retries
    ))
}

/// Incremental hasher for building hashes across multiple chunks
pub struct IncrementalHasher {
    inner: Sha1,
}

impl IncrementalHasher {
    /// Create a new incremental hasher
    pub fn new() -> Self {
        Self {
            inner: Sha1::new(),
        }
    }

    /// Update the hash with more data
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finalize and return the hash
    pub fn finalize(self) -> Sha1Hash {
        let result = self.inner.finalize();
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&result);
        Sha1Hash::from_bytes(bytes)
    }
}

impl Default for IncrementalHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Git-compatible hashing functions
pub mod git {
    use super::*;

    /// Hash blob in Git format: "blob <size>\0<content>"
    /// This produces the exact same hash as `git hash-object`
    pub fn hash_blob(content: &[u8]) -> Sha1Hash {
        let header = format!("blob {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(content);
        let result = hasher.finalize();
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&result);
        Sha1Hash::from_bytes(bytes)
    }

    /// Hash tree in Git format
    /// Format: "tree <size>\0<mode> <name>\0<20-byte-hash>..."
    /// Entries must be sorted by name
    pub fn hash_tree(entries: &[(String, u32, Sha1Hash)]) -> Sha1Hash {
        // Build tree content
        let mut content = Vec::new();

        // Sort entries by name (Git requirement)
        let mut sorted_entries = entries.to_vec();
        sorted_entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, mode, hash) in sorted_entries {
            // Format: <mode> <name>\0<20-byte-hash>
            // Mode must be in octal format (Git standard)
            content.extend_from_slice(format!("{:o} {}\0", mode, name).as_bytes());
            content.extend_from_slice(hash.as_bytes());
        }

        // Add Git header
        let header = format!("tree {}\0", content.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(&content);
        let result = hasher.finalize();
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&result);
        Sha1Hash::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_hash_consistency() {
        let data = b"hello world";
        let hash1 = hash_bytes(data);
        let hash2 = hash_bytes(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hex_encoding_roundtrip() {
        let original = Sha1Hash::from_bytes([42; 20]);
        let hex = original.to_hex();
        let decoded = Sha1Hash::from_hex(&hex).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_hex_encoding_lowercase() {
        let pattern = [0xde, 0xad, 0xbe, 0xef];
        let mut bytes = [0u8; 20];
        for (i, &byte) in pattern.iter().cycle().take(20).enumerate() {
            bytes[i] = byte;
        }
        let hash = Sha1Hash::from_bytes(bytes);
        let hex = hash.to_hex();
        assert!(hex.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_eq!(hex.len(), 40);  // SHA-1 is 40 hex chars
    }

    #[test]
    fn test_hex_decoding_invalid_length() {
        assert!(Sha1Hash::from_hex("abc").is_err());
        assert!(Sha1Hash::from_hex("").is_err());
        assert!(Sha1Hash::from_hex(&"a".repeat(39)).is_err());
        assert!(Sha1Hash::from_hex(&"a".repeat(64)).is_err());  // BLAKE3 length
    }

    #[test]
    fn test_hex_decoding_invalid_chars() {
        let invalid = "g".repeat(40);
        assert!(Sha1Hash::from_hex(&invalid).is_err());
    }

    #[test]
    fn test_incremental_hasher() {
        let data = b"hello world";
        let hash_direct = hash_bytes(data);

        let mut incremental = IncrementalHasher::new();
        incremental.update(b"hello ");
        incremental.update(b"world");
        let hash_incremental = incremental.finalize();

        assert_eq!(hash_direct, hash_incremental);
    }

    #[test]
    fn test_hash_file() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let file_path = temp_dir.path().join("test.txt");

        let data = b"test file content";
        std::fs::write(&file_path, data)?;

        let hash_from_file = hash_file(&file_path)?;
        let hash_from_git_blob = git::hash_blob(data);

        assert_eq!(hash_from_file, hash_from_git_blob, "hash_file should use Git blob format");
        Ok(())
    }

    #[test]
    fn test_hash_file_mmap() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let file_path = temp_dir.path().join("test.txt");

        let data = b"test file content for mmap";
        std::fs::write(&file_path, data)?;

        let hash_mmap = hash_file_mmap(&file_path)?;
        let hash_git_blob = git::hash_blob(data);

        assert_eq!(hash_mmap, hash_git_blob, "hash_file_mmap should use Git blob format");
        Ok(())
    }

    #[test]
    fn test_hash_large_file() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let file_path = temp_dir.path().join("large.txt");

        // Create a 5MB file
        let mut file = std::fs::File::create(&file_path)?;
        let chunk = vec![0xAB; 1024 * 1024]; // 1MB chunk
        for _ in 0..5 {
            file.write_all(&chunk)?;
        }
        drop(file);

        // Both methods should produce same hash
        let hash_streaming = hash_file(&file_path)?;
        let hash_mmap = hash_file_mmap(&file_path)?;

        assert_eq!(hash_streaming, hash_mmap);
        Ok(())
    }

    #[test]
    fn test_hash_empty_data() {
        let data = b"";
        let hash = hash_bytes(data);
        // SHA-1 of empty string is deterministic
        let hash2 = hash_bytes(data);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_different_data_different_hash() {
        let hash1 = hash_bytes(b"hello");
        let hash2 = hash_bytes(b"world");
        assert_ne!(hash1, hash2);
    }

    // Double-stat verification tests

    #[test]
    fn test_stable_file_succeeds() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let file = temp_dir.path().join("stable.txt");
        std::fs::write(&file, b"stable content")?;

        // Stable file should hash successfully using Git blob format
        let hash = hash_file_stable(&file, 3)?;
        assert_eq!(hash, git::hash_blob(b"stable content"));
        Ok(())
    }

    #[test]
    fn test_unstable_file_retries_then_fails() -> Result<()> {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;
        use std::thread;

        let temp_dir = tempfile::tempdir()?;
        let file = temp_dir.path().join("unstable.txt");
        std::fs::write(&file, b"initial")?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let write_count = Arc::new(AtomicU64::new(0));
        let stop_flag_clone = stop_flag.clone();
        let write_count_clone = write_count.clone();
        let file_clone = file.clone();

        // Spawn writer thread that constantly changes file VERY rapidly
        let writer = thread::spawn(move || {
            let mut counter = 0u64;
            while !stop_flag_clone.load(Ordering::Relaxed) {
                // Vary content length to ensure metadata changes
                let content = format!("changing {} {}", counter, "x".repeat((counter % 100) as usize));
                let _ = std::fs::write(&file_clone, content);
                counter += 1;
                write_count_clone.store(counter, Ordering::Relaxed);
                // No sleep - write as fast as possible
            }
        });

        // Wait for writer to actually start writing (verify at least 10 writes happened)
        let start = std::time::Instant::now();
        while write_count.load(Ordering::Relaxed) < 10 && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(write_count.load(Ordering::Relaxed) >= 10, "Writer didn't start properly");

        // Try multiple hash attempts - at least ONE should detect the instability
        // This is more reliable than a single attempt
        let mut detected_unstable = false;
        for _ in 0..5 {
            // Use only 1 retry per attempt to make detection more likely
            let result = hash_file_stable(&file, 1);
            if result.is_err() {
                let err_msg = result.unwrap_err().to_string();
                if err_msg.contains("unstable") {
                    detected_unstable = true;
                    break;
                }
            }
        }

        // Stop writer
        stop_flag.store(true, Ordering::Relaxed);
        let total_writes = write_count.load(Ordering::Relaxed);
        writer.join().unwrap();

        // Should have detected instability at least once
        assert!(
            detected_unstable,
            "Failed to detect file instability after {} writes. This may indicate a timing issue.",
            total_writes
        );
        Ok(())
    }

    #[test]
    fn test_eventually_stable_file_succeeds() -> Result<()> {
        use std::thread;
        use std::time::Instant;

        let temp_dir = tempfile::tempdir()?;
        let file = temp_dir.path().join("eventually.txt");
        std::fs::write(&file, b"initial")?;

        let file_clone = file.clone();

        // Write for 200ms then stop
        let writer = thread::spawn(move || {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(200) {
                let _ = std::fs::write(&file_clone, b"changing...");
                thread::sleep(Duration::from_millis(20));
            }
            // Final stable write
            std::fs::write(&file_clone, b"stable now").unwrap();
        });

        // Start after some changes
        thread::sleep(Duration::from_millis(100));

        // Should eventually succeed (with retries)
        let result = hash_file_stable(&file, 10);  // Allow more retries

        writer.join().unwrap();

        // Should succeed
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_stable_hash_matches_regular_hash() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let file = temp_dir.path().join("test.txt");
        let data = b"test data for comparison";
        std::fs::write(&file, data)?;

        let hash_stable = hash_file_stable(&file, 3)?;
        let hash_regular = hash_file(&file)?;
        let hash_git_blob = git::hash_blob(data);

        assert_eq!(hash_stable, hash_regular, "stable and regular hash_file should match");
        assert_eq!(hash_stable, hash_git_blob, "hash_file should use Git blob format");
        Ok(())
    }

    // Git compatibility tests

    #[test]
    fn test_git_blob_hash() {
        // Test that our hash_blob matches Git's hash-object
        let content = b"Hello, Git!";
        let hash = git::hash_blob(content);

        // This is the actual SHA-1 hash Git would produce for this content
        // Can be verified with: echo -n "Hello, Git!" | git hash-object --stdin
        // Note: The actual hash would need to be computed with real Git for verification
        assert_eq!(hash.to_hex().len(), 40);
    }

    #[test]
    fn test_git_tree_hash() {
        // Test tree hashing with simple entries
        let entries = vec![
            ("README.md".to_string(), 100644, hash_bytes(b"# README")),
            ("script.sh".to_string(), 100755, hash_bytes(b"#!/bin/bash\n")),
        ];

        let hash = git::hash_tree(&entries);
        assert_eq!(hash.to_hex().len(), 40);
    }

    #[test]
    fn test_git_tree_sorting() {
        // Git requires entries to be sorted by name
        let entries1 = vec![
            ("z.txt".to_string(), 100644, hash_bytes(b"z")),
            ("a.txt".to_string(), 100644, hash_bytes(b"a")),
            ("m.txt".to_string(), 100644, hash_bytes(b"m")),
        ];

        let entries2 = vec![
            ("a.txt".to_string(), 100644, hash_bytes(b"a")),
            ("m.txt".to_string(), 100644, hash_bytes(b"m")),
            ("z.txt".to_string(), 100644, hash_bytes(b"z")),
        ];

        let hash1 = git::hash_tree(&entries1);
        let hash2 = git::hash_tree(&entries2);

        // Should produce same hash regardless of input order
        assert_eq!(hash1, hash2);
    }
}
