//! Timelapse Core - Content-addressed storage primitives for Timelapse checkpoint system
//!
//! This crate provides the foundational storage layer:
//! - SHA-1 hashing (Git-compatible)
//! - Blob storage with compression
//! - Tree representation and diffing
//! - On-disk store management

pub mod hash;
pub mod blob;
pub mod tree;
pub mod store;

// Re-export main types for convenience
pub use hash::{Sha1Hash, IncrementalHasher};
pub use blob::{Blob, BlobStore};
pub use tree::{Tree, Entry, EntryKind, TreeDiff};
pub use store::Store;

/// Common result type used throughout timelapse-core
pub type Result<T> = anyhow::Result<T>;
