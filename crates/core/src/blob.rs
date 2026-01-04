//! Blob storage with compression and content-addressing

use crate::hash::Blake3Hash;
use anyhow::Result;
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Blob header format (version 1)
#[derive(Debug, Clone)]
pub struct BlobHeaderV1 {
    /// Magic bytes: "SNB1"
    pub magic: [u8; 4],
    /// Flags: bit0=compressed, bit1-7=reserved
    pub flags: u8,
    /// Original size (before compression)
    pub orig_len: u64,
    /// Stored size (after compression, if compressed)
    pub stored_len: u64,
}

impl BlobHeaderV1 {
    const MAGIC: [u8; 4] = *b"SNB1";
    const FLAG_COMPRESSED: u8 = 0b0000_0001;

    /// Create a new blob header
    pub fn new(orig_len: u64, stored_len: u64, compressed: bool) -> Self {
        let flags = if compressed { Self::FLAG_COMPRESSED } else { 0 };
        Self {
            magic: Self::MAGIC,
            flags,
            orig_len,
            stored_len,
        }
    }

    /// Check if blob is compressed
    pub fn is_compressed(&self) -> bool {
        (self.flags & Self::FLAG_COMPRESSED) != 0
    }

    /// Serialize header to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(21);
        bytes.extend_from_slice(&self.magic);
        bytes.push(self.flags);
        bytes.extend_from_slice(&self.orig_len.to_le_bytes());
        bytes.extend_from_slice(&self.stored_len.to_le_bytes());
        bytes
    }

    /// Deserialize header from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 21 {
            anyhow::bail!("Invalid header length: expected at least 21 bytes, got {}", bytes.len());
        }

        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != Self::MAGIC {
            anyhow::bail!("Invalid magic bytes: expected {:?}, got {:?}", Self::MAGIC, magic);
        }

        let flags = bytes[4];
        let orig_len = u64::from_le_bytes([
            bytes[5], bytes[6], bytes[7], bytes[8],
            bytes[9], bytes[10], bytes[11], bytes[12],
        ]);
        let stored_len = u64::from_le_bytes([
            bytes[13], bytes[14], bytes[15], bytes[16],
            bytes[17], bytes[18], bytes[19], bytes[20],
        ]);

        Ok(Self {
            magic,
            flags,
            orig_len,
            stored_len,
        })
    }
}

/// A blob represents a stored file's contents
#[derive(Debug, Clone)]
pub struct Blob {
    /// Content hash (BLAKE3)
    pub hash: Blake3Hash,
    /// Original size
    pub size: u64,
    /// Whether this blob is stored compressed
    pub compressed: bool,
}

impl Blob {
    /// Create a new blob from bytes
    pub fn from_bytes(data: &[u8]) -> Result<(Self, Vec<u8>)> {
        use crate::hash::hash_bytes;

        let hash = hash_bytes(data);
        let orig_len = data.len() as u64;

        // Decide if compression is worth it (> 4KB)
        let should_compress = data.len() > 4096;

        let (stored_data, stored_len, compressed) = if should_compress {
            match zstd::encode_all(data, 3) {
                Ok(compressed_data) => {
                    // Only use compression if it actually reduces size
                    if compressed_data.len() < data.len() {
                        let len = compressed_data.len() as u64;
                        (compressed_data, len, true)
                    } else {
                        (data.to_vec(), orig_len, false)
                    }
                }
                Err(_) => {
                    // If compression fails, store uncompressed
                    (data.to_vec(), orig_len, false)
                }
            }
        } else {
            (data.to_vec(), orig_len, false)
        };

        let header = BlobHeaderV1::new(orig_len, stored_len, compressed);
        let mut serialized = header.to_bytes();
        serialized.extend_from_slice(&stored_data);

        let blob = Blob {
            hash,
            size: orig_len,
            compressed,
        };

        Ok((blob, serialized))
    }

    /// Serialize blob with header
    pub fn to_bytes(&self, data: &[u8]) -> Result<Vec<u8>> {
        let orig_len = data.len() as u64;

        let (stored_data, stored_len) = if self.compressed {
            let compressed = zstd::encode_all(data, 3)?;
            let len = compressed.len() as u64;
            (compressed, len)
        } else {
            (data.to_vec(), orig_len)
        };

        let header = BlobHeaderV1::new(orig_len, stored_len, self.compressed);
        let mut serialized = header.to_bytes();
        serialized.extend_from_slice(&stored_data);

        Ok(serialized)
    }

    /// Read and decompress blob from serialized bytes (header + data)
    pub fn read_from_bytes(serialized: &[u8]) -> Result<Vec<u8>> {
        let header = BlobHeaderV1::from_bytes(serialized)?;

        let data_start = 21; // Header size
        let data_end = data_start + header.stored_len as usize;

        if serialized.len() < data_end {
            anyhow::bail!(
                "Invalid blob data length: expected at least {} bytes, got {}",
                data_end,
                serialized.len()
            );
        }

        let stored_data = &serialized[data_start..data_end];

        if header.is_compressed() {
            let decompressed = zstd::decode_all(stored_data)?;
            if decompressed.len() != header.orig_len as usize {
                anyhow::bail!(
                    "Decompressed size mismatch: expected {} bytes, got {}",
                    header.orig_len,
                    decompressed.len()
                );
            }
            Ok(decompressed)
        } else {
            Ok(stored_data.to_vec())
        }
    }
}

/// Blob storage with caching
pub struct BlobStore {
    /// Root directory for blob storage
    root: PathBuf,
    /// In-memory cache: hash -> blob metadata
    cache: DashMap<Blake3Hash, Arc<Blob>>,
    /// Maximum cache size in bytes (default: 50MB)
    max_cache_size: usize,
    // TODO: Add buffer pool for memory optimization
    // buffer_pool: BufferPool<BytesMut>,
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

    /// Write a blob to storage
    pub fn write_blob(&self, hash: Blake3Hash, data: &[u8]) -> Result<()> {
        use std::fs;
        use std::io::Write;

        // Check if blob already exists
        let blob_path = self.blob_path(hash);
        if blob_path.exists() {
            return Ok(()); // Already stored, idempotent
        }

        // Create blob with header
        let (blob, serialized) = Blob::from_bytes(data)?;

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
        temp_file.write_all(&serialized)?;
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
    pub fn read_blob(&self, hash: Blake3Hash) -> Result<Vec<u8>> {
        use std::fs;

        // Check cache first
        if let Some(cached_blob) = self.cache.get(&hash) {
            // Blob metadata is cached, but we need to read the actual data from disk
            // For now, we'll read from disk anyway (full caching would cache data too)
            drop(cached_blob); // Release the lock
        }

        // Read from disk
        let blob_path = self.blob_path(hash);
        if !blob_path.exists() {
            anyhow::bail!("Blob not found: {}", hash.to_hex());
        }

        let serialized = fs::read(&blob_path)?;

        // Decompress and verify
        let data = Blob::read_from_bytes(&serialized)?;

        // Verify hash matches
        let actual_hash = crate::hash::hash_bytes(&data);
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
    pub fn has_blob(&self, hash: Blake3Hash) -> bool {
        // Check cache first
        if self.cache.contains_key(&hash) {
            return true;
        }

        // Check filesystem
        self.blob_path(hash).exists()
    }

    /// Get the filesystem path for a blob
    fn blob_path(&self, hash: Blake3Hash) -> PathBuf {
        let hex = hash.to_hex();
        // Split: first 2 chars as prefix directory, rest as filename
        let prefix = &hex[0..2];
        let rest = &hex[2..];
        self.root.join("objects").join("blobs").join(prefix).join(rest)
    }

    // TODO: Implement LRU eviction
    // fn evict_if_needed(&self) { ... }

    // TODO: Implement buffer pool
    // fn get_buffer(&self) -> BytesMut { ... }
    // fn return_buffer(&self, buf: BytesMut) { ... }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_header_serialization() {
        let header = BlobHeaderV1::new(1000, 500, true);
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), 21);

        let parsed = BlobHeaderV1::from_bytes(&bytes).unwrap();
        assert_eq!(header.orig_len, parsed.orig_len);
        assert_eq!(header.stored_len, parsed.stored_len);
        assert_eq!(header.is_compressed(), parsed.is_compressed());
        assert_eq!(header.magic, parsed.magic);
    }

    #[test]
    fn test_blob_header_magic_validation() {
        let mut bytes = vec![0u8; 21];
        bytes[0..4].copy_from_slice(b"BADM"); // Wrong magic
        bytes[4] = 0;
        bytes[5..13].copy_from_slice(&1000u64.to_le_bytes());
        bytes[13..21].copy_from_slice(&500u64.to_le_bytes());

        assert!(BlobHeaderV1::from_bytes(&bytes).is_err());
    }

    #[test]
    fn test_blob_header_invalid_length() {
        let bytes = vec![0u8; 10]; // Too short
        assert!(BlobHeaderV1::from_bytes(&bytes).is_err());
    }

    #[test]
    fn test_blob_small_no_compression() {
        let data = b"hello world"; // < 4KB
        let (blob, serialized) = Blob::from_bytes(data).unwrap();

        assert!(!blob.compressed);
        assert_eq!(blob.size, data.len() as u64);

        // Verify header
        let header = BlobHeaderV1::from_bytes(&serialized).unwrap();
        assert!(!header.is_compressed());
        assert_eq!(header.orig_len, data.len() as u64);

        // Verify we can read it back
        let recovered = Blob::read_from_bytes(&serialized).unwrap();
        assert_eq!(data, &recovered[..]);
    }

    #[test]
    fn test_blob_large_with_compression() {
        // Create highly compressible data > 4KB
        let data = b"hello world ".repeat(1000); // ~12KB of repetitive data
        let (blob, serialized) = Blob::from_bytes(&data).unwrap();

        assert!(blob.compressed);
        assert_eq!(blob.size, data.len() as u64);

        // Verify compression actually reduced size
        assert!(serialized.len() < data.len());

        // Verify we can decompress correctly
        let recovered = Blob::read_from_bytes(&serialized).unwrap();
        assert_eq!(data, recovered);
    }

    #[test]
    fn test_blob_random_data_no_compression_benefit() {
        // Random data doesn't compress well, should stay uncompressed
        let mut data = vec![0u8; 5000];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 256) as u8; // Pseudo-random
        }

        let (blob, _serialized) = Blob::from_bytes(&data).unwrap();

        // May or may not be compressed depending on zstd behavior with this pattern
        // But decompression should work either way
        let (_blob2, serialized2) = Blob::from_bytes(&data).unwrap();
        let recovered = Blob::read_from_bytes(&serialized2).unwrap();
        assert_eq!(data, recovered);
    }

    #[test]
    fn test_blob_hash_deterministic() {
        use crate::hash::hash_bytes;

        let data = b"test data for hashing";
        let (blob1, _) = Blob::from_bytes(data).unwrap();
        let (blob2, _) = Blob::from_bytes(data).unwrap();

        assert_eq!(blob1.hash, blob2.hash);
        assert_eq!(blob1.hash, hash_bytes(data));
    }

    #[test]
    fn test_blob_header_compressed_flag() {
        let header_compressed = BlobHeaderV1::new(1000, 500, true);
        assert!(header_compressed.is_compressed());

        let header_uncompressed = BlobHeaderV1::new(1000, 1000, false);
        assert!(!header_uncompressed.is_compressed());
    }

    #[test]
    fn test_blob_empty_data() -> Result<()> {
        let data = b"";
        let (blob, serialized) = Blob::from_bytes(data)?;

        assert!(!blob.compressed); // Empty data shouldn't compress
        assert_eq!(blob.size, 0);

        let recovered = Blob::read_from_bytes(&serialized)?;
        assert_eq!(data, &recovered[..]);
        Ok(())
    }

    // BlobStore tests (Phase 1.3)

    #[test]
    fn test_blob_store_write_read_roundtrip() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data for blob store";
        let hash = hash_bytes(data);

        // Write blob
        store.write_blob(hash, data)?;

        // Read blob back
        let read_data = store.read_blob(hash)?;
        assert_eq!(data, &read_data[..]);

        Ok(())
    }

    #[test]
    fn test_blob_store_idempotent_writes() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data";
        let hash = hash_bytes(data);

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
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data";
        let hash = hash_bytes(data);

        // Should not exist before write
        assert!(!store.has_blob(hash));

        // Write blob
        store.write_blob(hash, data)?;

        // Should exist after write
        assert!(store.has_blob(hash));

        Ok(())
    }

    #[test]
    fn test_blob_store_file_structure() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"test data";
        let hash = hash_bytes(data);
        let hex = hash.to_hex();

        store.write_blob(hash, data)?;

        // Verify file structure: objects/blobs/<first2chars>/<rest>
        let expected_path = temp_dir.path()
            .join("objects")
            .join("blobs")
            .join(&hex[0..2])
            .join(&hex[2..]);

        assert!(expected_path.exists());

        Ok(())
    }

    #[test]
    fn test_blob_store_read_nonexistent() {
        use crate::hash::Blake3Hash;

        let temp_dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let fake_hash = Blake3Hash::from_bytes([0xFF; 32]);

        assert!(store.read_blob(fake_hash).is_err());
    }

    #[test]
    fn test_blob_store_large_blob() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        // Create large compressible data (20KB)
        let data = b"hello world ".repeat(2000);
        let hash = hash_bytes(&data);

        store.write_blob(hash, &data)?;
        let read_data = store.read_blob(hash)?;

        assert_eq!(data, read_data);

        Ok(())
    }

    #[test]
    fn test_blob_store_cache_hit() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"cached data";
        let hash = hash_bytes(data);

        store.write_blob(hash, data)?;

        // First read - populates cache
        store.read_blob(hash)?;

        // has_blob should use cache
        assert!(store.has_blob(hash));

        Ok(())
    }

    #[test]
    fn test_blob_store_multiple_blobs() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data1 = b"first blob";
        let data2 = b"second blob";
        let data3 = b"third blob";

        let hash1 = hash_bytes(data1);
        let hash2 = hash_bytes(data2);
        let hash3 = hash_bytes(data3);

        store.write_blob(hash1, data1)?;
        store.write_blob(hash2, data2)?;
        store.write_blob(hash3, data3)?;

        assert_eq!(data1, &store.read_blob(hash1)?[..]);
        assert_eq!(data2, &store.read_blob(hash2)?[..]);
        assert_eq!(data3, &store.read_blob(hash3)?[..]);

        Ok(())
    }

    #[test]
    fn test_blob_store_empty_blob() -> Result<()> {
        use crate::hash::hash_bytes;

        let temp_dir = tempfile::tempdir()?;
        let store = BlobStore::new(temp_dir.path().to_path_buf());

        let data = b"";
        let hash = hash_bytes(data);

        store.write_blob(hash, data)?;
        let read_data = store.read_blob(hash)?;

        assert_eq!(data, &read_data[..]);

        Ok(())
    }
}
