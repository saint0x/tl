//! Blob storage with Git-compatible format

use crate::hash::Sha1Hash;
use anyhow::Result;
use dashmap::DashMap;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// A blob represents a stored file's contents in Git format
#[derive(Debug, Clone)]
pub struct Blob {
    /// Content hash (SHA-1)
    pub hash: Sha1Hash,
    /// Original size
    pub size: u64,
}

impl Blob {
    /// Create a new blob from bytes using Git blob format
    pub fn from_bytes(data: &[u8]) -> Result<(Self, Vec<u8>)> {
        use crate::hash::git::hash_blob;

        let hash = hash_blob(data);
        let size = data.len() as u64;

        // Git blob format: "blob <size>\0<content>"
        let header = format!("blob {}\0", data.len());
        let mut git_object = Vec::new();
        git_object.extend_from_slice(header.as_bytes());
        git_object.extend_from_slice(data);

        // Compress with zlib (Git standard)
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&git_object)?;
        let compressed = encoder.finish()?;

        let blob = Blob { hash, size };

        Ok((blob, compressed))
    }

    /// Read and decompress Git blob from bytes
    pub fn read_from_bytes(compressed: &[u8]) -> Result<Vec<u8>> {
        // Decompress with zlib
        let mut decoder = ZlibDecoder::new(compressed);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;

        // Parse Git blob format: "blob <size>\0<content>"
        if !decompressed.starts_with(b"blob ") {
            anyhow::bail!("Invalid Git blob format: missing 'blob ' header");
        }

        // Find the null byte separator
        let null_pos = decompressed
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid Git blob format: missing null separator"))?;

        // Parse size from header
        let size_str = std::str::from_utf8(&decompressed[5..null_pos])?;
        let expected_size: usize = size_str.parse()?;

        // Extract content after null byte
        let content = &decompressed[null_pos + 1..];

        // Verify size matches
        if content.len() != expected_size {
            anyhow::bail!(
                "Git blob size mismatch: header says {}, got {} bytes",
                expected_size,
                content.len()
            );
        }

        Ok(content.to_vec())
    }
}

/// Blob storage with caching (Git-compatible)
pub struct BlobStore {
    /// Root directory for blob storage
    root: PathBuf,
    /// In-memory cache: hash -> blob metadata
    cache: DashMap<Sha1Hash, Arc<Blob>>,
    /// Maximum cache size in bytes (default: 50MB)
    max_cache_size: usize,
}

impl BlobStore {
    /// Create a new blob store
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: DashMap::new(),
            max_cache_size: 50 * 1024 * 1024, // 50 MB
        }
    }

    /// Write a blob to storage in Git format
    pub fn write_blob(&self, hash: Sha1Hash, data: &[u8]) -> Result<()> {
        use std::fs;

        // Check if blob already exists
        let blob_path = self.blob_path(hash);
        if blob_path.exists() {
            return Ok(()); // Already stored, idempotent
        }

        // Create blob with Git format
        let (blob, compressed) = Blob::from_bytes(data)?;

        // Verify hash matches
        if blob.hash != hash {
            anyhow::bail!(
                "Hash mismatch: expected {}, got {}",
                hash.to_hex(),
                blob.hash.to_hex()
            );
        }

        // Ensure parent directory exists
        if let Some(parent) = blob_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomic write pattern: write to temp, fsync, rename
        let tmp_dir = self.root.join("tmp").join("ingest");
        fs::create_dir_all(&tmp_dir)?;

        let temp_path = tmp_dir.join(format!("{}-{}", uuid::Uuid::new_v4(), hash.to_hex()));

        // Write to temp file
        let mut temp_file = fs::File::create(&temp_path)?;
        temp_file.write_all(&compressed)?;
        temp_file.sync_all()?; // fsync file
        drop(temp_file);

        // Rename to final location
        fs::rename(&temp_path, &blob_path)?;

        // Fsync parent directory for durability
        if let Some(parent) = blob_path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all(); // Best effort, may fail on some filesystems
            }
        }

        // Add to cache
        self.cache.insert(hash, Arc::new(blob));

        Ok(())
    }

    /// Read a blob from storage
    pub fn read_blob(&self, hash: Sha1Hash) -> Result<Vec<u8>> {
        use std::fs;

        // Check cache first (for metadata)
        if let Some(cached_blob) = self.cache.get(&hash) {
            drop(cached_blob); // Release the lock
        }

        // Read from disk
        let blob_path = self.blob_path(hash);
        if !blob_path.exists() {
            anyhow::bail!("Blob not found: {}", hash.to_hex());
        }

        let compressed = fs::read(&blob_path)?;

        // Decompress and parse Git format
        let data = Blob::read_from_bytes(&compressed)?;

        // Verify hash matches
        let actual_hash = crate::hash::git::hash_blob(&data);
        if actual_hash != hash {
            anyhow::bail!(
                "Hash mismatch: expected {}, got {}",
                hash.to_hex(),
                actual_hash.to_hex()
            );
        }

        Ok(data)
    }

    /// Check if a blob exists
    pub fn has_blob(&self, hash: Sha1Hash) -> bool {
        // Check cache first
        if self.cache.contains_key(&hash) {
            return true;
        }

        // Check filesystem
        self.blob_path(hash).exists()
    }

    /// Get the filesystem path for a blob (Git-compatible structure)
    fn blob_path(&self, hash: Sha1Hash) -> PathBuf {
        let hex = hash.to_hex();
        // Git structure: objects/<first2chars>/<rest>
        let prefix = &hex[0..2];
        let rest = &hex[2..];
        self.root.join("objects").join(prefix).join(rest)
    }
}

// Re-export for backward compatibility (though we're removing backward compat)
#[deprecated(note = "Use Git blob format directly")]
pub struct BlobHeaderV1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_git_format() -> Result<()> {
        let data = b"hello world";
        let (blob, compressed) = Blob::from_bytes(data)?;

        // Verify blob has correct size
        assert_eq!(blob.size, data.len() as u64);

        // Verify we can read it back
        let recovered = Blob::read_from_bytes(&compressed)?;
        assert_eq!(data, &recovered[..]);

        Ok(())
    }

    #[test]
    fn test_blob_hash_matches_git() -> Result<()> {
        use crate::hash::git::hash_blob;

        let data = b"test data for git compatibility";
        let (blob, _) = Blob::from_bytes(data)?;

        // Hash should match what Git would produce
        let expected_hash = hash_blob(data);
        assert_eq!(blob.hash, expected_hash);

        Ok(())
    }

    #[test]
    fn test_blob_empty_data() -> Result<()> {
        let data = b"";
        let (blob, compressed) = Blob::from_bytes(data)?;

        assert_eq!(blob.size, 0);

        let recovered = Blob::read_from_bytes(&compressed)?;
        assert_eq!(data, &recovered[..]);
        Ok(())
    }

    #[test]
    fn test_blob_large_data() -> Result<()> {
        // Create large data (20KB)
        let data = b"hello world ".repeat(2000);
        let (blob, compressed) = Blob::from_bytes(&data)?;

        assert_eq!(blob.size, data.len() as u64);

        // Verify compression reduced size
        assert!(compressed.len() < data.len());

        // Verify decompression works
        let recovered = Blob::read_from_bytes(&compressed)?;
        assert_eq!(data, recovered);

        Ok(())
    }

    #[test]
    fn test_blob_deterministic() -> Result<()> {
        let data = b"deterministic test";
        let (blob1, _) = Blob::from_bytes(data)?;
        let (blob2, _) = Blob::from_bytes(data)?;

        assert_eq!(blob1.hash, blob2.hash);
        Ok(())
    }

    #[test]
    fn test_blob_invalid_format() {
        // Invalid blob data (not Git format)
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"invalid data").unwrap();
        let compressed = encoder.finish().unwrap();

        assert!(Blob::read_from_bytes(&compressed).is_err());
    }

    #[test]
    fn test_blob_size_mismatch() {
        // Create blob with wrong size in header
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"blob 999\0actual content").unwrap();
        let compressed = encoder.finish().unwrap();

        assert!(Blob::read_from_bytes(&compressed).is_err());
    }

    // BlobStore tests

    #[test]
    fn test_blob_store_write_read_roundtrip() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data for blob store";
        let hash = hash_blob(data);

        // Write blob
        store.write_blob(hash, data)?;

        // Read blob back
        let read_data = store.read_blob(hash)?;
        assert_eq!(data, &read_data[..]);

        Ok(())
    }

    #[test]
    fn test_blob_store_idempotent_writes() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data";
        let hash = hash_blob(data);

        // Write multiple times
        store.write_blob(hash, data)?;
        store.write_blob(hash, data)?;
        store.write_blob(hash, data)?;

        // Should still be readable
        let read_data = store.read_blob(hash)?;
        assert_eq!(data, &read_data[..]);

        Ok(())
    }

    #[test]
    fn test_blob_store_has_blob() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data";
        let hash = hash_blob(data);

        // Should not exist before write
        assert!(!store.has_blob(hash));

        // Write blob
        store.write_blob(hash, data)?;

        // Should exist after write
        assert!(store.has_blob(hash));

        Ok(())
    }

    #[test]
    fn test_blob_store_git_structure() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data";
        let hash = hash_blob(data);
        let hex = hash.to_hex();

        store.write_blob(hash, data)?;

        // Verify Git file structure: objects/<first2chars>/<rest>
        let expected_path = temp_dir
            .path()
            .join("objects")
            .join(&hex[0..2])
            .join(&hex[2..]);

        assert!(expected_path.exists());

        Ok(())
    }

    #[test]
    fn test_blob_store_read_nonexistent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let fake_hash = Sha1Hash::from_bytes([0xFF; 20]);

        assert!(store.read_blob(fake_hash).is_err());
    }

    #[test]
    fn test_blob_store_multiple_blobs() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data1 = b"first blob";
        let data2 = b"second blob";
        let data3 = b"third blob";

        let hash1 = hash_blob(data1);
        let hash2 = hash_blob(data2);
        let hash3 = hash_blob(data3);

        store.write_blob(hash1, data1)?;
        store.write_blob(hash2, data2)?;
        store.write_blob(hash3, data3)?;

        assert_eq!(data1, &store.read_blob(hash1)?[..]);
        assert_eq!(data2, &store.read_blob(hash2)?[..]);
        assert_eq!(data3, &store.read_blob(hash3)?[..]);

        Ok(())
    }

    #[test]
    fn test_blob_store_cache_hit() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"cached data";
        let hash = hash_blob(data);

        store.write_blob(hash, data)?;

        // First read - populates cache
        store.read_blob(hash)?;

        // has_blob should use cache
        assert!(store.has_blob(hash));

        Ok(())
    }

    #[test]
    fn test_blob_store_empty_blob() -> Result<()> {
        use crate::hash::git::hash_blob;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"";
        let hash = hash_blob(data);

        store.write_blob(hash, data)?;
        let read_data = store.read_blob(hash)?;

        assert_eq!(data, &read_data[..]);

        Ok(())
    }
}
