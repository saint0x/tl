//! Checkpoint ↔ JJ commit ID mapping
//!
//! This module provides bidirectional mapping between Timelapse checkpoint IDs (ULIDs)
//! and JJ commit IDs (hex strings). The mapping is persisted in a sled database at
//! `.tl/state/jj-mapping/`.
//!
//! The mapping enables:
//! - Finding which JJ commit corresponds to a checkpoint (for incremental publishing)
//! - Finding which checkpoint corresponds to a JJ commit (for import on pull)
//! - Storing the seed commit for fast initial publishes
//! - Verifying mapping integrity

use anyhow::{Context, Result};
use std::path::Path;
use ulid::Ulid;

/// Special key for the seed commit mapping.
/// The seed commit represents the initial repository state at init time,
/// enabling incremental tree conversion even for the first publish.
pub const SEED_COMMIT_KEY: &str = "SEED_INIT";

// Note: We alias our core crate as tl_core in Cargo.toml to avoid conflicts with std::core

/// Bidirectional mapping between checkpoint IDs and JJ commit IDs
pub struct JjMapping {
    db: sled::Db,
}

impl JjMapping {
    /// Open the mapping database at `.tl/state/jj-mapping/`
    ///
    /// Creates the database if it doesn't exist.
    pub fn open(tl_dir: &Path) -> Result<Self> {
        let db_path = tl_dir.join("state/jj-mapping");
        let db = sled::open(&db_path)
            .with_context(|| format!("Failed to open JJ mapping database at {}", db_path.display()))?;

        Ok(Self { db })
    }

    /// Store mapping: checkpoint_id → jj_commit_id
    ///
    /// This is the forward mapping, used when we publish a checkpoint to JJ
    /// and want to remember which JJ commit it became.
    ///
    /// NOTE: Does not flush to disk. Call flush() after batch operations.
    pub fn set(&self, checkpoint_id: Ulid, jj_commit_id: &str) -> Result<()> {
        let key = checkpoint_id.to_string();
        self.db.insert(key.as_bytes(), jj_commit_id.as_bytes())
            .context("Failed to store checkpoint → JJ commit mapping")?;
        Ok(())
    }

    /// Get JJ commit ID for a checkpoint
    ///
    /// Returns None if the checkpoint has not been published to JJ.
    pub fn get_jj_commit(&self, checkpoint_id: Ulid) -> Result<Option<String>> {
        let key = checkpoint_id.to_string();
        if let Some(value) = self.db.get(key.as_bytes())
            .context("Failed to query JJ mapping database")? {
            let commit_id = String::from_utf8(value.to_vec())
                .context("Invalid UTF-8 in stored JJ commit ID")?;
            return Ok(Some(commit_id));
        }
        Ok(None)
    }

    /// Store reverse mapping: jj_commit_id → checkpoint_id
    ///
    /// This is used when importing JJ commits to find if we already have
    /// a checkpoint for this commit.
    ///
    /// NOTE: Does not flush to disk. Call flush() after batch operations.
    pub fn set_reverse(&self, jj_commit_id: &str, checkpoint_id: Ulid) -> Result<()> {
        let key = format!("rev:{}", jj_commit_id);
        let value = checkpoint_id.to_string();
        self.db.insert(key.as_bytes(), value.as_bytes())
            .context("Failed to store JJ commit → checkpoint mapping")?;
        Ok(())
    }

    /// Get checkpoint ID for a JJ commit
    ///
    /// Returns None if the JJ commit has not been imported as a checkpoint.
    pub fn get_checkpoint(&self, jj_commit_id: &str) -> Result<Option<Ulid>> {
        let key = format!("rev:{}", jj_commit_id);
        if let Some(value) = self.db.get(key.as_bytes())
            .context("Failed to query JJ mapping database")? {
            let id_str = String::from_utf8(value.to_vec())
                .context("Invalid UTF-8 in stored checkpoint ID")?;
            let checkpoint_id = Ulid::from_string(&id_str)
                .context("Invalid ULID in stored checkpoint ID")?;
            return Ok(Some(checkpoint_id));
        }
        Ok(None)
    }

    /// Remove mapping for a checkpoint
    ///
    /// This is useful for garbage collection or resetting mappings.
    pub fn remove(&self, checkpoint_id: Ulid) -> Result<()> {
        let key = checkpoint_id.to_string();

        // Get JJ commit ID so we can remove reverse mapping too
        if let Some(jj_commit_id) = self.get_jj_commit(checkpoint_id)? {
            let rev_key = format!("rev:{}", jj_commit_id);
            self.db.remove(rev_key.as_bytes())
                .context("Failed to remove reverse mapping")?;
        }

        self.db.remove(key.as_bytes())
            .context("Failed to remove forward mapping")?;
        self.db.flush()?;

        Ok(())
    }

    /// Get all mapped checkpoints
    ///
    /// Returns an iterator of (checkpoint_id, jj_commit_id) pairs.
    /// Only includes forward mappings (not reverse).
    pub fn all_mappings(&self) -> Result<Vec<(Ulid, String)>> {
        let mut mappings = Vec::new();

        for item in self.db.iter() {
            let (key, value) = item.context("Failed to iterate mappings")?;
            let key_str = String::from_utf8(key.to_vec())
                .context("Invalid UTF-8 in mapping key")?;

            // Skip reverse mappings (they start with "rev:")
            if key_str.starts_with("rev:") {
                continue;
            }

            let checkpoint_id = Ulid::from_string(&key_str)
                .context("Invalid ULID in mapping key")?;
            let jj_commit_id = String::from_utf8(value.to_vec())
                .context("Invalid UTF-8 in mapping value")?;

            mappings.push((checkpoint_id, jj_commit_id));
        }

        Ok(mappings)
    }

    /// Count of total mappings
    pub fn count(&self) -> usize {
        // Divide by 2 because we store both forward and reverse mappings
        self.db.len() / 2
    }

    /// Flush all pending writes to disk
    ///
    /// Call this after a batch of set/set_reverse operations to ensure
    /// durability. This performs a single fsync instead of one per write.
    pub fn flush(&self) -> Result<()> {
        self.db.flush()
            .map(|_| ()) // Discard the usize return value
            .context("Failed to flush JJ mapping database")
    }

    /// Clear all mappings (useful for testing or reset)
    pub fn clear(&self) -> Result<()> {
        self.db.clear()
            .context("Failed to clear JJ mapping database")?;
        self.db.flush()?;
        Ok(())
    }

    /// Store the seed commit ID
    ///
    /// The seed commit represents the initial repository state at init time.
    /// This enables incremental tree conversion for root checkpoints.
    ///
    /// NOTE: Does not flush to disk. Call flush() after batch operations.
    pub fn set_seed(&self, jj_commit_id: &str) -> Result<()> {
        self.db.insert(SEED_COMMIT_KEY.as_bytes(), jj_commit_id.as_bytes())
            .context("Failed to store seed commit mapping")?;
        Ok(())
    }

    /// Get the seed commit ID
    ///
    /// Returns None if no seed commit has been created yet.
    pub fn get_seed(&self) -> Result<Option<String>> {
        if let Some(value) = self.db.get(SEED_COMMIT_KEY.as_bytes())
            .context("Failed to query seed commit")? {
            let commit_id = String::from_utf8(value.to_vec())
                .context("Invalid UTF-8 in stored seed commit ID")?;
            return Ok(Some(commit_id));
        }
        Ok(None)
    }

    /// Check if seed commit exists
    ///
    /// Fast check without loading the full commit ID.
    pub fn has_seed(&self) -> Result<bool> {
        Ok(self.db.contains_key(SEED_COMMIT_KEY.as_bytes())
            .context("Failed to check seed commit existence")?)
    }

    /// Store the seed tree hash (TL tree format)
    ///
    /// This is the Timelapse tree hash at init time, used for computing
    /// tree diffs when publishing without a published parent.
    ///
    /// NOTE: Does not flush to disk. Call flush() after batch operations.
    pub fn set_seed_tree(&self, tree_hash: &str) -> Result<()> {
        let key = format!("{}_TREE", SEED_COMMIT_KEY);
        self.db.insert(key.as_bytes(), tree_hash.as_bytes())
            .context("Failed to store seed tree hash")?;
        Ok(())
    }

    /// Get the seed tree hash (TL tree format)
    ///
    /// Returns None if no seed tree has been stored.
    pub fn get_seed_tree(&self) -> Result<Option<String>> {
        let key = format!("{}_TREE", SEED_COMMIT_KEY);
        if let Some(value) = self.db.get(key.as_bytes())
            .context("Failed to query seed tree hash")? {
            let tree_hash = String::from_utf8(value.to_vec())
                .context("Invalid UTF-8 in stored seed tree hash")?;
            return Ok(Some(tree_hash));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_mapping_roundtrip() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mapping = JjMapping::open(temp_dir.path())?;

        let checkpoint_id = Ulid::new();
        let jj_commit_id = "abc123def456";

        // Store forward and reverse mappings
        mapping.set(checkpoint_id, jj_commit_id)?;
        mapping.set_reverse(jj_commit_id, checkpoint_id)?;

        // Verify forward lookup
        let found_commit = mapping.get_jj_commit(checkpoint_id)?;
        assert_eq!(found_commit, Some(jj_commit_id.to_string()));

        // Verify reverse lookup
        let found_checkpoint = mapping.get_checkpoint(jj_commit_id)?;
        assert_eq!(found_checkpoint, Some(checkpoint_id));

        Ok(())
    }

    #[test]
    fn test_missing_mapping() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mapping = JjMapping::open(temp_dir.path())?;

        let checkpoint_id = Ulid::new();

        // Should return None for unmapped checkpoint
        assert_eq!(mapping.get_jj_commit(checkpoint_id)?, None);

        Ok(())
    }

    #[test]
    fn test_remove_mapping() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mapping = JjMapping::open(temp_dir.path())?;

        let checkpoint_id = Ulid::new();
        let jj_commit_id = "abc123";

        mapping.set(checkpoint_id, jj_commit_id)?;
        mapping.set_reverse(jj_commit_id, checkpoint_id)?;

        // Verify it exists
        assert!(mapping.get_jj_commit(checkpoint_id)?.is_some());

        // Remove it
        mapping.remove(checkpoint_id)?;

        // Verify it's gone (both forward and reverse)
        assert_eq!(mapping.get_jj_commit(checkpoint_id)?, None);
        assert_eq!(mapping.get_checkpoint(jj_commit_id)?, None);

        Ok(())
    }

    #[test]
    fn test_all_mappings() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mapping = JjMapping::open(temp_dir.path())?;

        let cp1 = Ulid::new();
        let cp2 = Ulid::new();

        mapping.set(cp1, "commit1")?;
        mapping.set_reverse("commit1", cp1)?;
        mapping.set(cp2, "commit2")?;
        mapping.set_reverse("commit2", cp2)?;

        let all = mapping.all_mappings()?;
        assert_eq!(all.len(), 2);
        assert_eq!(mapping.count(), 2);

        Ok(())
    }

    #[test]
    fn test_seed_commit() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mapping = JjMapping::open(temp_dir.path())?;

        // Initially no seed
        assert!(!mapping.has_seed()?);
        assert_eq!(mapping.get_seed()?, None);

        // Set seed
        let seed_id = "abc123seed456";
        mapping.set_seed(seed_id)?;

        // Verify seed exists
        assert!(mapping.has_seed()?);
        assert_eq!(mapping.get_seed()?, Some(seed_id.to_string()));

        Ok(())
    }
}
