//! Checkpoint journal and state management
//!
//! This crate provides:
//! - Checkpoint data structures (ULID-based IDs)
//! - Append-only journal (sled embedded DB)
//! - PathMap state cache
//! - Incremental tree update algorithm
//! - Retention policies & GC

pub mod checkpoint;
pub mod journal;
pub mod pathmap;
pub mod incremental;
pub mod retention;
pub mod recovery;

// Re-exports
pub use checkpoint::{Checkpoint, CheckpointMeta, CheckpointReason};
pub use journal::Journal;
pub use pathmap::PathMap;
pub use incremental::incremental_update;
pub use retention::{GarbageCollector, GcMetrics, PinManager, RetentionPolicy};
pub use recovery::recover_on_startup;

/// Result type for journal operations
pub type Result<T> = anyhow::Result<T>;
