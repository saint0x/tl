//! Append-only checkpoint journal using sled

use crate::Checkpoint;
use anyhow::Result;
use parking_lot::RwLock;
use sled::Db;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use ulid::Ulid;

/// Append-only journal for checkpoints
pub struct Journal {
    /// Sled database
    db: Db,
    /// In-memory index: checkpoint_id -> sequence_number
    index: RwLock<BTreeMap<Ulid, u64>>,
    /// Monotonic sequence counter
    seq_counter: AtomicU64,
}

impl Journal {
    /// Open or create a journal at the given path
    pub fn open(path: &Path) -> Result<Self> {
        let db = sled::open(path.join("checkpoints.db"))?;

        // Build in-memory index on startup
        let mut index = BTreeMap::new();
        let mut max_seq = 0u64;

        for item in db.iter() {
            let (key, value) = item?;
            let seq = u64::from_le_bytes(key.as_ref().try_into()?);
            let checkpoint = Checkpoint::deserialize(&value)?;
            index.insert(checkpoint.id, seq);
            max_seq = max_seq.max(seq);
        }

        Ok(Self {
            db,
            index: RwLock::new(index),
            seq_counter: AtomicU64::new(max_seq + 1),
        })
    }

    /// Append a checkpoint to the journal
    pub fn append(&self, checkpoint: &Checkpoint) -> Result<u64> {
        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst);
        let key = seq.to_le_bytes();
        let value = checkpoint.serialize()?;

        self.db.insert(&key, value)?;

        // Update index
        self.index.write().insert(checkpoint.id, seq);

        // Flush to ensure durability
        self.db.flush()?;

        Ok(seq)
    }

    /// Get a checkpoint by ID
    pub fn get(&self, id: &Ulid) -> Result<Option<Checkpoint>> {
        let seq = match self.index.read().get(id) {
            Some(&seq) => seq,
            None => return Ok(None),
        };

        let key = seq.to_le_bytes();
        let value = match self.db.get(&key)? {
            Some(v) => v,
            None => return Ok(None),
        };

        Ok(Some(Checkpoint::deserialize(&value)?))
    }

    /// Get the latest checkpoint
    pub fn latest(&self) -> Result<Option<Checkpoint>> {
        let index = self.index.read();
        if index.is_empty() {
            return Ok(None);
        }

        let max_seq = index.values().max().copied().unwrap();
        drop(index);

        let key = max_seq.to_le_bytes();
        let value = self.db.get(&key)?.unwrap();
        Ok(Some(Checkpoint::deserialize(&value)?))
    }

    /// Get the last N checkpoints
    pub fn last_n(&self, count: usize) -> Result<Vec<Checkpoint>> {
        let index = self.index.read();
        let mut seqs: Vec<_> = index.values().copied().collect();
        seqs.sort_unstable();

        let start_idx = seqs.len().saturating_sub(count);
        let recent_seqs = &seqs[start_idx..];

        drop(index);

        let mut checkpoints = Vec::new();
        for &seq in recent_seqs {
            let key = seq.to_le_bytes();
            let value = self.db.get(&key)?.unwrap();
            checkpoints.push(Checkpoint::deserialize(&value)?);
        }

        Ok(checkpoints)
    }

    /// Get checkpoints since a timestamp
    pub fn since(&self, timestamp_ms: u64) -> Result<Vec<Checkpoint>> {
        let index = self.index.read();

        let mut checkpoints = Vec::new();
        for (&id, &seq) in index.iter() {
            let ulid_ts_ms = id.timestamp_ms();
            if ulid_ts_ms >= timestamp_ms {
                let key = seq.to_le_bytes();
                let value = self.db.get(&key)?.unwrap();
                checkpoints.push(Checkpoint::deserialize(&value)?);
            }
        }

        Ok(checkpoints)
    }

    /// Get all checkpoint IDs
    pub fn all_checkpoint_ids(&self) -> Result<HashSet<Ulid>> {
        Ok(self.index.read().keys().copied().collect())
    }

    /// Delete a checkpoint
    pub fn delete(&self, id: &Ulid) -> Result<()> {
        let seq = match self.index.write().remove(id) {
            Some(seq) => seq,
            None => return Ok(()), // Already deleted
        };

        let key = seq.to_le_bytes();
        self.db.remove(&key)?;
        Ok(())
    }

    /// Get the total number of checkpoints
    pub fn count(&self) -> usize {
        self.index.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckpointMeta, CheckpointReason};
    use core::Blake3Hash;
    use tempfile::TempDir;

    fn create_test_checkpoint(parent: Option<Ulid>) -> Checkpoint {
        let root_tree = Blake3Hash::from_bytes([1u8; 32]);
        let meta = CheckpointMeta {
            files_changed: 1,
            bytes_added: 100,
            bytes_removed: 0,
        };

        Checkpoint::new(parent, root_tree, CheckpointReason::FsBatch, vec![], meta)
    }

    #[test]
    fn test_journal_open_and_append() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        assert_eq!(journal.count(), 0);

        // Append a checkpoint (sequences start at 1)
        let checkpoint = create_test_checkpoint(None);
        let seq = journal.append(&checkpoint)?;

        assert_eq!(seq, 1);
        assert_eq!(journal.count(), 1);

        Ok(())
    }

    #[test]
    fn test_journal_get_latest() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        // Initially empty
        assert!(journal.latest()?.is_none());

        // Add checkpoints
        let cp1 = create_test_checkpoint(None);
        journal.append(&cp1)?;

        let latest = journal.latest()?.unwrap();
        assert_eq!(latest.id, cp1.id);

        // Add second checkpoint
        let cp2 = create_test_checkpoint(Some(cp1.id));
        journal.append(&cp2)?;

        let latest = journal.latest()?.unwrap();
        assert_eq!(latest.id, cp2.id);

        Ok(())
    }

    #[test]
    fn test_journal_get_by_id() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        let checkpoint = create_test_checkpoint(None);
        journal.append(&checkpoint)?;

        // Get by ID
        let retrieved = journal.get(&checkpoint.id)?.unwrap();
        assert_eq!(retrieved.id, checkpoint.id);
        assert_eq!(retrieved.root_tree, checkpoint.root_tree);

        // Non-existent ID
        let random_id = Ulid::new();
        assert!(journal.get(&random_id)?.is_none());

        Ok(())
    }

    #[test]
    fn test_journal_last_n() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        // Add 10 checkpoints
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        for i in 0..10 {
            let parent = if i == 0 { None } else { Some(checkpoints[i - 1].id) };
            let cp = create_test_checkpoint(parent);
            journal.append(&cp)?;
            checkpoints.push(cp);
        }

        // Get last 5
        let last_5 = journal.last_n(5)?;
        assert_eq!(last_5.len(), 5);

        // Returns in chronological order (oldest to newest)
        for i in 0..5 {
            assert_eq!(last_5[i].id, checkpoints[5 + i].id);
        }

        // Get more than exist
        let last_20 = journal.last_n(20)?;
        assert_eq!(last_20.len(), 10);

        Ok(())
    }

    #[test]
    fn test_journal_since_timestamp() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        // Add some checkpoints with delays
        let cp1 = create_test_checkpoint(None);
        journal.append(&cp1)?;

        std::thread::sleep(std::time::Duration::from_millis(10));

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;

        std::thread::sleep(std::time::Duration::from_millis(10));

        let cp2 = create_test_checkpoint(Some(cp1.id));
        journal.append(&cp2)?;

        // Get checkpoints since timestamp (should only get cp2)
        let since = journal.since(timestamp)?;
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].id, cp2.id);

        Ok(())
    }

    #[test]
    fn test_journal_all_checkpoint_ids() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        let mut expected_ids = HashSet::new();

        // Add checkpoints
        for _ in 0..5 {
            let cp = create_test_checkpoint(None);
            expected_ids.insert(cp.id);
            journal.append(&cp)?;
        }

        let all_ids = journal.all_checkpoint_ids()?;
        assert_eq!(all_ids, expected_ids);

        Ok(())
    }

    #[test]
    fn test_journal_delete() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        let checkpoint = create_test_checkpoint(None);
        journal.append(&checkpoint)?;

        assert_eq!(journal.count(), 1);

        // Delete checkpoint
        journal.delete(&checkpoint.id)?;

        assert_eq!(journal.count(), 0);
        assert!(journal.get(&checkpoint.id)?.is_none());

        Ok(())
    }

    #[test]
    fn test_journal_persistence() -> Result<()> {
        let temp_dir = TempDir::new()?;

        let checkpoint_id = {
            let journal = Journal::open(temp_dir.path())?;
            let checkpoint = create_test_checkpoint(None);
            journal.append(&checkpoint)?;
            checkpoint.id
        };

        // Reopen journal
        let journal = Journal::open(temp_dir.path())?;
        assert_eq!(journal.count(), 1);

        let retrieved = journal.get(&checkpoint_id)?.unwrap();
        assert_eq!(retrieved.id, checkpoint_id);

        Ok(())
    }

    #[test]
    fn test_journal_multiple_appends() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let journal = Journal::open(temp_dir.path())?;

        // Append 100 checkpoints (sequences start at 1)
        for i in 0..100 {
            let cp = create_test_checkpoint(None);
            let seq = journal.append(&cp)?;
            assert_eq!(seq, (i + 1) as u64);
        }

        assert_eq!(journal.count(), 100);

        Ok(())
    }
}
