//! Retention policies and garbage collection

use anyhow::Result;
use core::{EntryKind, Sha1Hash, Store};
use crate::Journal;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use ulid::Ulid;

/// Retention policy configuration
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// Number of checkpoints to keep (default: 2000)
    pub retain_dense_count: usize,
    /// Time window to keep dense checkpoints (default: 24h)
    pub retain_dense_window_ms: u64,
    /// Always retain pinned checkpoints
    pub retain_pins: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            retain_dense_count: 2000,
            retain_dense_window_ms: 24 * 60 * 60 * 1000, // 24 hours
            retain_pins: true,
        }
    }
}

/// Pin manager for named checkpoints
pub struct PinManager {
    pins_dir: PathBuf,
}

/// Stash entry metadata
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StashEntry {
    /// Checkpoint ID of the stashed state
    pub checkpoint_id: Ulid,
    /// Timestamp when stash was created
    pub created_at_ms: u64,
    /// Optional message describing the stash
    pub message: Option<String>,
    /// Checkpoint ID that was HEAD when stash was created (for restore)
    pub base_checkpoint_id: Option<Ulid>,
}

/// Stash manager for temporary working directory saves
///
/// Similar pattern to PinManager but stores richer metadata.
/// Used by auto-stash in pull and manual `tl stash` command.
pub struct StashManager {
    stash_dir: PathBuf,
}

impl StashManager {
    /// Create a new StashManager
    pub fn new(tl_dir: &Path) -> Self {
        Self {
            stash_dir: tl_dir.join("state/stash"),
        }
    }

    /// Push a new stash entry
    ///
    /// Returns the stash index (0 = most recent)
    pub fn push(&self, entry: StashEntry) -> Result<usize> {
        fs::create_dir_all(&self.stash_dir)?;

        // Get next index
        let existing = self.list()?;
        let index = existing.len();

        // Write stash file as JSON
        let stash_path = self.stash_dir.join(format!("stash-{}", index));
        let json = serde_json::to_string_pretty(&entry)?;

        let mut file = fs::File::create(&stash_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;

        Ok(index)
    }

    /// Pop the most recent stash (removes and returns it)
    pub fn pop(&self) -> Result<Option<StashEntry>> {
        let stashes = self.list()?;
        if stashes.is_empty() {
            return Ok(None);
        }

        // Get most recent (highest index)
        let (index, entry) = stashes.into_iter().last().unwrap();

        // Remove the file
        let stash_path = self.stash_dir.join(format!("stash-{}", index));
        if stash_path.exists() {
            fs::remove_file(&stash_path)?;
        }

        Ok(Some(entry))
    }

    /// Get stash by index without removing
    pub fn get(&self, index: usize) -> Result<Option<StashEntry>> {
        let stash_path = self.stash_dir.join(format!("stash-{}", index));

        if !stash_path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&stash_path)?;
        let entry: StashEntry = serde_json::from_str(&contents)?;
        Ok(Some(entry))
    }

    /// List all stashes (index, entry) sorted by index
    pub fn list(&self) -> Result<Vec<(usize, StashEntry)>> {
        if !self.stash_dir.exists() {
            return Ok(Vec::new());
        }

        let mut stashes = Vec::new();

        for dir_entry in fs::read_dir(&self.stash_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();

            if path.is_file() {
                let filename = dir_entry.file_name().to_string_lossy().to_string();
                if let Some(index_str) = filename.strip_prefix("stash-") {
                    if let Ok(index) = index_str.parse::<usize>() {
                        let contents = fs::read_to_string(&path)?;
                        if let Ok(entry) = serde_json::from_str::<StashEntry>(&contents) {
                            stashes.push((index, entry));
                        }
                    }
                }
            }
        }

        // Sort by index
        stashes.sort_by_key(|(idx, _)| *idx);
        Ok(stashes)
    }

    /// Drop a stash by index
    pub fn drop(&self, index: usize) -> Result<bool> {
        let stash_path = self.stash_dir.join(format!("stash-{}", index));

        if stash_path.exists() {
            fs::remove_file(&stash_path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Clear all stashes
    pub fn clear(&self) -> Result<usize> {
        let stashes = self.list()?;
        let count = stashes.len();

        for (index, _) in stashes {
            self.drop(index)?;
        }

        Ok(count)
    }

    /// Check if there are any stashes
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.list()?.is_empty())
    }

    /// Get the most recent stash without removing
    pub fn peek(&self) -> Result<Option<StashEntry>> {
        let stashes = self.list()?;
        Ok(stashes.into_iter().last().map(|(_, entry)| entry))
    }
}

impl PinManager {
    /// Create a new PinManager
    pub fn new(tl_dir: &Path) -> Self {
        Self {
            pins_dir: tl_dir.join("refs/pins"),
        }
    }

    /// Pin a checkpoint with a name
    pub fn pin(&self, name: &str, checkpoint_id: Ulid) -> Result<()> {
        // Validate pin name (alphanumeric + hyphens/underscores only)
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("Invalid pin name: must be alphanumeric with hyphens/underscores");
        }

        // Ensure pins directory exists
        fs::create_dir_all(&self.pins_dir)?;

        // Write ULID to pin file
        let pin_path = self.pins_dir.join(name);
        let ulid_str = checkpoint_id.to_string();

        // Atomic write (not critical for pins, but good practice)
        let mut file = fs::File::create(&pin_path)?;
        file.write_all(ulid_str.as_bytes())?;
        file.sync_all()?;

        Ok(())
    }

    /// Remove a pin
    pub fn unpin(&self, name: &str) -> Result<()> {
        let pin_path = self.pins_dir.join(name);

        // Idempotent: OK if file doesn't exist
        if pin_path.exists() {
            fs::remove_file(&pin_path)?;
        }

        Ok(())
    }

    /// List all pins
    pub fn list_pins(&self) -> Result<Vec<(String, Ulid)>> {
        if !self.pins_dir.exists() {
            return Ok(Vec::new());
        }

        let mut pins = Vec::new();

        for entry in fs::read_dir(&self.pins_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                let contents = fs::read_to_string(&path)?;
                let ulid = Ulid::from_string(contents.trim())?;
                pins.push((name, ulid));
            }
        }

        Ok(pins)
    }

    /// Get all pinned checkpoint IDs (for GC mark phase)
    pub fn get_pinned_checkpoints(&self) -> Result<HashSet<Ulid>> {
        let pins = self.list_pins()?;
        Ok(pins.into_iter().map(|(_, id)| id).collect())
    }
}

/// GC metrics
#[derive(Debug, Clone, Default)]
pub struct GcMetrics {
    pub checkpoints_deleted: usize,
    pub trees_deleted: usize,
    pub blobs_deleted: usize,
    pub bytes_freed: u64,
    pub duration_ms: u64,
}

impl GcMetrics {
    pub fn log_summary(&self) {
        eprintln!(
            "GC completed: {} checkpoints, {} trees, {} blobs deleted",
            self.checkpoints_deleted, self.trees_deleted, self.blobs_deleted
        );
        eprintln!(
            "Space freed: {:.2} MB in {} ms",
            self.bytes_freed as f64 / (1024.0 * 1024.0),
            self.duration_ms
        );
    }
}

/// Garbage collector
pub struct GarbageCollector {
    policy: RetentionPolicy,
}

impl GarbageCollector {
    /// Create a new GC with the given policy
    pub fn new(policy: RetentionPolicy) -> Self {
        Self { policy }
    }

    /// Run garbage collection
    ///
    /// Optionally accepts workspace checkpoint IDs to protect them from deletion.
    pub fn collect(
        &self,
        journal: &Journal,
        store: &Store,
        pin_manager: &PinManager,
        workspace_checkpoints: Option<&HashSet<Ulid>>,
    ) -> Result<GcMetrics> {
        let start_time = SystemTime::now();
        let mut metrics = GcMetrics::default();

        // Phase 1: Mark live checkpoints
        let live_checkpoints = self.mark_live_checkpoints(journal, pin_manager, workspace_checkpoints)?;

        // Phase 2: Mark live objects (trees and blobs)
        let (live_trees, live_blobs) =
            self.mark_live_objects(&live_checkpoints, journal, store)?;

        // Phase 3: Sweep dead objects
        self.sweep_dead_objects(
            &live_checkpoints,
            &live_trees,
            &live_blobs,
            journal,
            store,
            &mut metrics,
        )?;

        metrics.duration_ms = start_time.elapsed()?.as_millis() as u64;
        metrics.log_summary();

        Ok(metrics)
    }

    /// Mark live checkpoints based on retention policy
    fn mark_live_checkpoints(
        &self,
        journal: &Journal,
        pin_manager: &PinManager,
        workspace_checkpoints: Option<&HashSet<Ulid>>,
    ) -> Result<HashSet<Ulid>> {
        let mut live = HashSet::new();

        // Criterion 1: Pinned checkpoints
        if self.policy.retain_pins {
            live.extend(pin_manager.get_pinned_checkpoints()?);
        }

        // Criterion 2: Workspace checkpoints (always protected)
        if let Some(ws_checkpoints) = workspace_checkpoints {
            live.extend(ws_checkpoints.iter().copied());
        }

        // Criterion 3: Last N checkpoints
        let recent = journal.last_n(self.policy.retain_dense_count)?;
        live.extend(recent.iter().map(|cp| cp.id));

        // Criterion 4: Checkpoints within time window
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        let cutoff_ms = now_ms.saturating_sub(self.policy.retain_dense_window_ms);
        let recent_by_time = journal.since(cutoff_ms)?;
        live.extend(recent_by_time.iter().map(|cp| cp.id));

        Ok(live)
    }

    /// Mark live objects (trees and blobs) referenced by live checkpoints
    ///
    /// CRITICAL: This now recursively walks all subtrees to mark nested blobs.
    /// Previously, only direct entries were marked, causing nested directory
    /// blobs to be deleted by GC while still referenced.
    fn mark_live_objects(
        &self,
        live_checkpoints: &HashSet<Ulid>,
        journal: &Journal,
        store: &Store,
    ) -> Result<(HashSet<Sha1Hash>, HashSet<Sha1Hash>)> {
        let mut live_trees = HashSet::new();
        let mut live_blobs = HashSet::new();

        for cp_id in live_checkpoints {
            if let Some(checkpoint) = journal.get(cp_id)? {
                // Recursively walk the tree and mark all objects
                Self::mark_tree_recursive(
                    checkpoint.root_tree,
                    &mut live_trees,
                    &mut live_blobs,
                    store,
                )?;
            }
        }

        Ok((live_trees, live_blobs))
    }

    /// Recursively walk a tree and mark all referenced objects as live
    ///
    /// CRITICAL: This now verifies blob existence before marking as live.
    /// Missing blobs are logged as warnings but don't halt GC.
    fn mark_tree_recursive(
        tree_hash: Sha1Hash,
        live_trees: &mut HashSet<Sha1Hash>,
        live_blobs: &mut HashSet<Sha1Hash>,
        store: &Store,
    ) -> Result<()> {
        // Avoid re-processing trees we've already seen (handles potential cycles)
        if !live_trees.insert(tree_hash) {
            return Ok(());
        }

        let tree = store.read_tree(tree_hash)?;

        for entry in tree.entries() {
            match entry.kind {
                EntryKind::Tree => {
                    // Recurse into subtree - entry.blob_hash is actually a tree hash
                    Self::mark_tree_recursive(entry.blob_hash, live_trees, live_blobs, store)?;
                }
                EntryKind::File | EntryKind::ExecutableFile | EntryKind::Symlink => {
                    // CRITICAL: Verify blob exists before marking as live
                    // This catches corrupted/missing blobs early
                    if store.blob_store().has_blob(entry.blob_hash) {
                        live_blobs.insert(entry.blob_hash);
                    } else {
                        // Log warning but don't fail - blob might have been legitimately deleted
                        tracing::warn!(
                            "Missing blob referenced by tree {}: {}",
                            tree_hash.to_hex(),
                            entry.blob_hash.to_hex()
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Sweep dead objects (delete unreferenced checkpoints, trees, blobs)
    fn sweep_dead_objects(
        &self,
        live_checkpoints: &HashSet<Ulid>,
        live_trees: &HashSet<Sha1Hash>,
        live_blobs: &HashSet<Sha1Hash>,
        journal: &Journal,
        store: &Store,
        metrics: &mut GcMetrics,
    ) -> Result<()> {
        // Delete unreferenced checkpoints
        let all_checkpoints = journal.all_checkpoint_ids()?;
        for cp_id in all_checkpoints {
            if !live_checkpoints.contains(&cp_id) {
                journal.delete(&cp_id)?;
                metrics.checkpoints_deleted += 1;
            }
        }

        // Delete unreferenced trees
        for tree_hash in enumerate_stored_trees(store)? {
            if !live_trees.contains(&tree_hash) {
                delete_tree(store, tree_hash)?;
                metrics.trees_deleted += 1;
            }
        }

        // Delete unreferenced blobs (with size tracking)
        for blob_hash in enumerate_stored_blobs(store)? {
            if !live_blobs.contains(&blob_hash) {
                let blob_size = get_blob_size(store, blob_hash)?;
                delete_blob(store, blob_hash)?;
                metrics.blobs_deleted += 1;
                metrics.bytes_freed += blob_size;
            }
        }

        Ok(())
    }
}

/// Enumerate all stored trees
fn enumerate_stored_trees(store: &Store) -> Result<Vec<Sha1Hash>> {
    let trees_dir = store.tl_dir().join("objects/trees");
    let mut hashes = Vec::new();

    if !trees_dir.exists() {
        return Ok(hashes);
    }

    // Walk through fan-out structure (objects/trees/ab/cdef...)
    for prefix_entry in fs::read_dir(&trees_dir)? {
        let prefix_entry = prefix_entry?;
        let prefix_path = prefix_entry.path();

        if prefix_path.is_dir() {
            for file_entry in fs::read_dir(&prefix_path)? {
                let file_entry = file_entry?;
                let file_path = file_entry.path();

                if file_path.is_file() {
                    // Reconstruct hash from prefix + filename
                    let prefix = prefix_entry.file_name().to_string_lossy().to_string();
                    let filename = file_entry.file_name().to_string_lossy().to_string();
                    let hex = format!("{}{}", prefix, filename);

                    if let Ok(hash) = Sha1Hash::from_hex(&hex) {
                        hashes.push(hash);
                    }
                }
            }
        }
    }

    Ok(hashes)
}

/// Enumerate all stored blobs
fn enumerate_stored_blobs(store: &Store) -> Result<Vec<Sha1Hash>> {
    let blobs_dir = store.tl_dir().join("objects/blobs");
    let mut hashes = Vec::new();

    if !blobs_dir.exists() {
        return Ok(hashes);
    }

    // Walk through fan-out structure (objects/blobs/ab/cdef...)
    for prefix_entry in fs::read_dir(&blobs_dir)? {
        let prefix_entry = prefix_entry?;
        let prefix_path = prefix_entry.path();

        if prefix_path.is_dir() {
            for file_entry in fs::read_dir(&prefix_path)? {
                let file_entry = file_entry?;
                let file_path = file_entry.path();

                if file_path.is_file() {
                    // Reconstruct hash from prefix + filename
                    let prefix = prefix_entry.file_name().to_string_lossy().to_string();
                    let filename = file_entry.file_name().to_string_lossy().to_string();
                    let hex = format!("{}{}", prefix, filename);

                    if let Ok(hash) = Sha1Hash::from_hex(&hex) {
                        hashes.push(hash);
                    }
                }
            }
        }
    }

    Ok(hashes)
}

/// Get blob size
fn get_blob_size(store: &Store, hash: Sha1Hash) -> Result<u64> {
    let hex = hash.to_hex();
    let (prefix, rest) = hex.split_at(2);
    let blob_path = store
        .tl_dir()
        .join("objects/blobs")
        .join(prefix)
        .join(rest);

    let metadata = fs::metadata(blob_path)?;
    Ok(metadata.len())
}

/// Delete a tree
fn delete_tree(store: &Store, hash: Sha1Hash) -> Result<()> {
    let hex = hash.to_hex();
    let (prefix, rest) = hex.split_at(2);
    let tree_path = store
        .tl_dir()
        .join("objects/trees")
        .join(prefix)
        .join(rest);

    if tree_path.exists() {
        fs::remove_file(tree_path)?;
    }

    Ok(())
}

/// Delete a blob
fn delete_blob(store: &Store, hash: Sha1Hash) -> Result<()> {
    let hex = hash.to_hex();
    let (prefix, rest) = hex.split_at(2);
    let blob_path = store
        .tl_dir()
        .join("objects/blobs")
        .join(prefix)
        .join(rest);

    if blob_path.exists() {
        fs::remove_file(blob_path)?;
    }

    Ok(())
}
