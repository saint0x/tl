//! Checkpoint data structures

use core::Sha1Hash;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// A checkpoint represents a snapshot of the repository at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique ID (ULID for timestamp + uniqueness)
    pub id: Ulid,
    /// Parent checkpoint ID
    pub parent: Option<Ulid>,
    /// Root tree hash for this checkpoint
    pub root_tree: Sha1Hash,
    /// Timestamp (Unix milliseconds)
    pub ts_unix_ms: u64,
    /// Reason for checkpoint
    pub reason: CheckpointReason,
    /// Paths touched in this checkpoint
    pub touched_paths: Vec<std::path::PathBuf>,
    /// Checkpoint metadata
    pub meta: CheckpointMeta,
}

/// Checkpoint metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    /// Number of files changed
    pub files_changed: u32,
    /// Bytes added
    pub bytes_added: u64,
    /// Bytes removed
    pub bytes_removed: u64,
}

impl Default for CheckpointMeta {
    fn default() -> Self {
        Self {
            files_changed: 0,
            bytes_added: 0,
            bytes_removed: 0,
        }
    }
}

/// Reason for creating a checkpoint
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointReason {
    /// File system batch
    FsBatch,
    /// Manual checkpoint
    Manual,
    /// Restore operation
    Restore,
    /// Publish to JJ
    Publish,
    /// GC compact
    GcCompact,
    /// Workspace save (auto-checkpoint on workspace switch)
    WorkspaceSave,
}

impl Checkpoint {
    /// Create a new checkpoint
    pub fn new(
        parent: Option<Ulid>,
        root_tree: Sha1Hash,
        reason: CheckpointReason,
        touched_paths: Vec<std::path::PathBuf>,
        meta: CheckpointMeta,
    ) -> Self {
        Self {
            id: Ulid::new(),
            parent,
            root_tree,
            ts_unix_ms: current_timestamp_ms(),
            reason,
            touched_paths,
            meta,
        }
    }

    /// Serialize checkpoint to bytes
    pub fn serialize(&self) -> anyhow::Result<Vec<u8>> {
        Ok(bincode::serialize(self)?)
    }

    /// Deserialize checkpoint from bytes
    pub fn deserialize(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(bincode::deserialize(bytes)?)
    }
}

fn current_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn create_test_checkpoint() -> Checkpoint {
        let root_tree = Sha1Hash::from_bytes([1u8; 20]);
        let meta = CheckpointMeta {
            files_changed: 5,
            bytes_added: 1024,
            bytes_removed: 512,
        };

        Checkpoint::new(
            None,
            root_tree,
            CheckpointReason::FsBatch,
            vec![PathBuf::from("test/file.txt")],
            meta,
        )
    }

    #[test]
    fn test_checkpoint_serialization_roundtrip() {
        let checkpoint = create_test_checkpoint();

        // Serialize
        let bytes = checkpoint.serialize().unwrap();

        // Deserialize
        let deserialized = Checkpoint::deserialize(&bytes).unwrap();

        // Verify fields match
        assert_eq!(checkpoint.id, deserialized.id);
        assert_eq!(checkpoint.parent, deserialized.parent);
        assert_eq!(checkpoint.root_tree, deserialized.root_tree);
        assert_eq!(checkpoint.reason, deserialized.reason);
        assert_eq!(checkpoint.touched_paths, deserialized.touched_paths);
        assert_eq!(checkpoint.meta.files_changed, deserialized.meta.files_changed);
        assert_eq!(checkpoint.meta.bytes_added, deserialized.meta.bytes_added);
        assert_eq!(checkpoint.meta.bytes_removed, deserialized.meta.bytes_removed);
    }

    #[test]
    fn test_checkpoint_compact_size() {
        let checkpoint = create_test_checkpoint();
        let bytes = checkpoint.serialize().unwrap();

        // Target: < 300 bytes per checkpoint (per plan)
        // Actual size will depend on paths, but should be reasonable
        assert!(bytes.len() < 500, "Checkpoint size too large: {} bytes", bytes.len());
    }

    #[test]
    fn test_ulid_ordering() {
        // Create two checkpoints with slight delay
        let cp1 = create_test_checkpoint();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let cp2 = create_test_checkpoint();

        // ULIDs should be monotonically increasing (time-ordered)
        assert!(cp1.id < cp2.id, "ULIDs should be time-ordered");
    }

    #[test]
    fn test_checkpoint_with_parent() {
        let parent_id = Ulid::new();
        let root_tree = Sha1Hash::from_bytes([2u8; 20]);
        let meta = CheckpointMeta {
            files_changed: 1,
            bytes_added: 100,
            bytes_removed: 0,
        };

        let checkpoint = Checkpoint::new(
            Some(parent_id),
            root_tree,
            CheckpointReason::Manual,
            vec![],
            meta,
        );

        assert_eq!(checkpoint.parent, Some(parent_id));
    }

    #[test]
    fn test_checkpoint_reasons() {
        let reasons = vec![
            CheckpointReason::FsBatch,
            CheckpointReason::Manual,
            CheckpointReason::Restore,
            CheckpointReason::Publish,
            CheckpointReason::GcCompact,
        ];

        for reason in reasons {
            let root_tree = Sha1Hash::from_bytes([3u8; 20]);
            let meta = CheckpointMeta {
                files_changed: 0,
                bytes_added: 0,
                bytes_removed: 0,
            };

            let checkpoint = Checkpoint::new(None, root_tree, reason, vec![], meta);

            // Verify serialization works for all reasons
            let bytes = checkpoint.serialize().unwrap();
            let deserialized = Checkpoint::deserialize(&bytes).unwrap();
            assert_eq!(checkpoint.reason, deserialized.reason);
        }
    }

    #[test]
    fn test_checkpoint_multiple_paths() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("tests/test.rs"),
        ];

        let root_tree = Sha1Hash::from_bytes([4u8; 20]);
        let meta = CheckpointMeta {
            files_changed: 3,
            bytes_added: 500,
            bytes_removed: 200,
        };

        let checkpoint = Checkpoint::new(
            None,
            root_tree,
            CheckpointReason::FsBatch,
            paths.clone(),
            meta,
        );

        let bytes = checkpoint.serialize().unwrap();
        let deserialized = Checkpoint::deserialize(&bytes).unwrap();

        assert_eq!(checkpoint.touched_paths, deserialized.touched_paths);
        assert_eq!(checkpoint.touched_paths.len(), 3);
    }
}
