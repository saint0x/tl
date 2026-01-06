//! File system watching for Timelapse
//!
//! This crate provides platform-specific file system watching with:
//! - Per-path debouncing (300ms default, configurable)
//! - Event coalescing and deduplication
//! - Overflow recovery with targeted rescan
//! - Path interning for memory optimization

pub mod platform;
pub mod debounce;
pub mod coalesce;
pub mod overflow;
pub mod reconcile;
pub mod ignore;

use anyhow::Result;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Configuration for the file system watcher
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Debounce duration per path (default: 300ms)
    pub debounce_duration: Duration,

    /// Batch size for emitting debounced events (default: 100)
    pub batch_size: usize,

    /// Batch timeout for emitting accumulated events (default: 100ms)
    pub batch_timeout: Duration,

    /// FSEvents latency on macOS (default: 50ms)
    pub fsevent_latency: Duration,

    /// Enable atomic save pattern detection (default: true)
    pub detect_atomic_saves: bool,

    /// Overflow recovery strategy
    pub overflow_strategy: OverflowStrategy,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce_duration: Duration::from_millis(300),
            batch_size: 100,
            batch_timeout: Duration::from_millis(100),
            fsevent_latency: Duration::from_millis(50),
            detect_atomic_saves: true,
            overflow_strategy: OverflowStrategy::PauseAndRescan,
        }
    }
}

/// Strategy for recovering from overflow conditions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowStrategy {
    /// Pause watcher, perform targeted rescan, resume
    PauseAndRescan,

    /// Continue watching, mark repository as dirty for next checkpoint
    ContinueAndMarkDirty,

    /// Restart watcher completely (last resort)
    RestartWatcher,
}

/// Path interning for memory-efficient storage
///
/// Deduplicates path allocations by storing a single Arc<Path> per unique path.
/// Thread-safe using DashMap for concurrent access.
pub struct PathInterner {
    paths: DashMap<PathBuf, Arc<Path>>,
}

impl PathInterner {
    /// Create a new path interner
    pub fn new() -> Self {
        Self {
            paths: DashMap::new(),
        }
    }

    /// Intern a path, returning an Arc<Path>
    ///
    /// If the path has been interned before, returns the existing Arc.
    /// Otherwise, creates a new Arc and stores it.
    pub fn intern(&self, path: impl AsRef<Path>) -> Arc<Path> {
        let path = path.as_ref();

        // Fast path: check if already interned
        if let Some(entry) = self.paths.get(path) {
            return Arc::clone(entry.value());
        }

        // Slow path: insert new path
        let path_buf = path.to_path_buf();
        let arc_path: Arc<Path> = Arc::from(path);

        self.paths
            .entry(path_buf)
            .or_insert_with(|| arc_path.clone())
            .clone()
    }

    /// Get the number of unique paths interned
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Check if the interner is empty
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Clear all interned paths (for testing/cleanup)
    pub fn clear(&self) {
        self.paths.clear();
    }
}

impl Default for PathInterner {
    fn default() -> Self {
        Self::new()
    }
}

/// File system event with interned paths
#[derive(Debug, Clone)]
pub struct WatchEvent {
    /// Path that changed (interned for memory efficiency)
    pub path: Arc<Path>,

    /// Type of change
    pub kind: EventKind,
}

impl WatchEvent {
    /// Create a new watch event
    pub fn new(path: Arc<Path>, kind: EventKind) -> Self {
        Self { path, kind }
    }
}

/// Type of file system event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// File created
    Create,

    /// File modified
    Modify(ModifyKind),

    /// File deleted
    Delete,

    /// File renamed
    Rename,

    /// Watcher buffer overflow - events may have been lost
    Overflow,
}

/// Kind of modification event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifyKind {
    /// File data changed
    Data,

    /// File metadata changed (permissions, timestamps, etc)
    Metadata,

    /// Unknown or any modification
    Any,
}

// =============================================================================
// Watcher Metrics (Fix 15)
// =============================================================================

/// Metrics for watcher operations
///
/// Tracks overflow events and recovery statistics for monitoring
/// and debugging file system watcher behavior.
#[derive(Debug, Default)]
pub struct WatcherMetrics {
    /// Total number of overflow events detected
    overflow_count: AtomicU64,

    /// Number of paths recovered from overflow events
    recovered_paths_total: AtomicU64,

    /// Total events processed
    events_processed: AtomicU64,
}

impl WatcherMetrics {
    /// Create new metrics
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an overflow event
    pub fn record_overflow(&self) {
        self.overflow_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record recovered paths count
    pub fn record_recovery(&self, path_count: u64) {
        self.recovered_paths_total.fetch_add(path_count, Ordering::Relaxed);
    }

    /// Record event processed
    pub fn record_event(&self) {
        self.events_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Get total overflow count
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
    }

    /// Get total recovered paths
    pub fn recovered_paths_total(&self) -> u64 {
        self.recovered_paths_total.load(Ordering::Relaxed)
    }

    /// Get total events processed
    pub fn events_processed(&self) -> u64 {
        self.events_processed.load(Ordering::Relaxed)
    }

    /// Get snapshot of all metrics
    pub fn snapshot(&self) -> WatcherMetricsSnapshot {
        WatcherMetricsSnapshot {
            overflow_count: self.overflow_count(),
            recovered_paths_total: self.recovered_paths_total(),
            events_processed: self.events_processed(),
        }
    }
}

/// Snapshot of watcher metrics at a point in time
#[derive(Debug, Clone, Copy)]
pub struct WatcherMetricsSnapshot {
    pub overflow_count: u64,
    pub recovered_paths_total: u64,
    pub events_processed: u64,
}

/// File system watcher coordinator
///
/// Orchestrates the event pipeline:
/// Platform Watcher → Normalizer → Coalescer → Debouncer → Output
pub struct Watcher {
    /// Root path being watched
    root: PathBuf,

    /// Watcher configuration
    config: WatcherConfig,

    /// Path interner for memory optimization
    interner: Arc<PathInterner>,

    /// Platform-specific watcher
    platform_watcher: Option<Box<dyn platform::PlatformWatcher>>,

    /// Event coalescer
    coalescer: coalesce::Coalescer,

    /// Event debouncer
    debouncer: debounce::Debouncer,

    /// Overflow recovery
    overflow_recovery: overflow::OverflowRecovery,

    /// Running state
    is_running: bool,

    /// Watcher metrics (Fix 15)
    metrics: WatcherMetrics,
}

impl Watcher {
    /// Create a new watcher for the given path
    pub fn new(path: &Path) -> Result<Self> {
        Self::with_config(path, WatcherConfig::default())
    }

    /// Create a new watcher with custom configuration
    pub fn with_config(path: &Path, config: WatcherConfig) -> Result<Self> {
        let debouncer = debounce::Debouncer::with_config(
            config.debounce_duration,
            config.batch_size,
            config.batch_timeout,
        );

        let coalescer = coalesce::Coalescer::with_config(
            config.batch_timeout,
            config.detect_atomic_saves,
        );

        let overflow_recovery = overflow::OverflowRecovery::new(path);

        Ok(Self {
            root: path.to_path_buf(),
            config,
            interner: Arc::new(PathInterner::new()),
            platform_watcher: None,
            coalescer,
            debouncer,
            overflow_recovery,
            is_running: false,
            metrics: WatcherMetrics::new(),
        })
    }

    /// Get the root path being watched
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the watcher configuration
    pub fn config(&self) -> &WatcherConfig {
        &self.config
    }

    /// Get a reference to the path interner
    pub fn interner(&self) -> &Arc<PathInterner> {
        &self.interner
    }

    /// Get watcher metrics (Fix 15)
    ///
    /// Returns metrics about watcher operations including overflow events,
    /// recovered paths, and total events processed.
    pub fn metrics(&self) -> &WatcherMetrics {
        &self.metrics
    }

    /// Start watching for events
    ///
    /// Initializes the platform watcher and begins monitoring the file system.
    /// Events are processed through the coalescing and debouncing pipeline.
    pub async fn start(&mut self) -> Result<()> {
        if self.is_running {
            return Ok(());
        }

        // Create platform watcher
        let mut platform_watcher = platform::create_platform_watcher(&self.root, &self.config)?;

        // Start platform watcher
        platform_watcher.start().await?;

        self.platform_watcher = Some(platform_watcher);
        self.is_running = true;

        Ok(())
    }

    /// Stop watching
    ///
    /// Stops the platform watcher and flushes any pending events.
    pub async fn stop(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }

        // Stop platform watcher
        if let Some(mut watcher) = self.platform_watcher.take() {
            watcher.stop().await?;
        }

        self.is_running = false;
        Ok(())
    }

    /// Poll for events and process through pipeline (non-blocking)
    ///
    /// Returns paths that are ready after debouncing.
    /// Should be called regularly (e.g., in a loop with tokio::time::sleep).
    pub async fn poll_events(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }

        let watcher = match self.platform_watcher.as_mut() {
            Some(w) => w,
            None => return Ok(()),
        };

        // Check for overflow
        if watcher.has_overflow() {
            // Record overflow metric (Fix 15)
            self.metrics.record_overflow();
            tracing::warn!(
                "Watcher overflow detected (total: {})",
                self.metrics.overflow_count()
            );

            // Perform targeted recovery
            let recovered_paths = self.overflow_recovery.recover()?;
            let recovered_count = recovered_paths.len();

            // Feed recovered paths to debouncer
            for path in recovered_paths {
                self.debouncer.push(path);
            }

            // Record recovery metric (Fix 15)
            self.metrics.record_recovery(recovered_count as u64);
            tracing::info!(
                "Overflow recovery: {} paths recovered (total: {})",
                recovered_count,
                self.metrics.recovered_paths_total()
            );

            // Reset overflow flag
            watcher.reset_overflow();
        }

        // Poll for events from platform watcher
        let mut processed_any = false;
        while let Some(event) = watcher.poll_event().await? {
            processed_any = true;

            // Record event processed metric (Fix 15)
            self.metrics.record_event();

            // Handle overflow events
            if matches!(event.kind, EventKind::Overflow) {
                // Will be handled on next poll
                continue;
            }

            // Intern the path
            let interned_path = self.interner.intern(event.path.as_ref());

            // Create new event with interned path
            let interned_event = WatchEvent::new(interned_path, event.kind);

            // Push to coalescer
            if let Some(immediate_event) = self.coalescer.push(interned_event) {
                // Atomic save detected or other immediate emission
                self.debouncer.push(immediate_event.path);
            }
        }

        // Collect coalesced events that are ready
        let ready_events = self.coalescer.collect_ready();
        for event in ready_events {
            self.debouncer.push(event.path);
            processed_any = true;
        }

        // If no events were processed, sleep briefly to prevent tight loop
        // This ensures poll_events() always awaits at least once
        if !processed_any {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        Ok(())
    }

    /// Get next batch of debounced paths (non-blocking)
    ///
    /// Returns paths that have completed the debounce period.
    /// Returns empty vec if no paths are ready.
    pub fn next_batch(&mut self) -> Vec<Arc<Path>> {
        self.debouncer.try_recv_batch()
    }

    /// Get next batch of debounced paths (async, with timeout)
    ///
    /// Waits up to batch_timeout for paths to be ready.
    pub async fn next_batch_async(&mut self) -> Vec<Arc<Path>> {
        self.debouncer.recv_batch().await
    }

    /// Flush all pending events immediately
    ///
    /// Returns all paths currently in the coalescing and debouncing pipeline.
    /// Useful for shutdown or forced checkpointing.
    pub fn flush(&mut self) -> Vec<Arc<Path>> {
        let mut all_paths = Vec::new();

        // Flush coalescer
        let coalesced = self.coalescer.flush();
        for event in coalesced {
            all_paths.push(event.path);
        }

        // Flush debouncer
        all_paths.extend(self.debouncer.flush());

        all_paths
    }

    /// Check if the watcher is currently running
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// Update the last checkpoint time for overflow recovery
    pub fn mark_checkpoint(&mut self, time: std::time::SystemTime) {
        self.overflow_recovery.mark_checkpoint(time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_interner_deduplication() {
        let interner = PathInterner::new();

        let path1 = interner.intern("foo/bar.txt");
        let path2 = interner.intern("foo/bar.txt");
        let path3 = interner.intern("foo/baz.txt");

        // Same path should return same Arc (pointer equality)
        assert!(Arc::ptr_eq(&path1, &path2));

        // Different paths should have different Arcs
        assert!(!Arc::ptr_eq(&path1, &path3));

        // Should have 2 unique paths
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn test_path_interner_thread_safety() {
        use std::thread;

        let interner = Arc::new(PathInterner::new());
        let mut handles = vec![];

        // Spawn 10 threads, each interning the same 100 paths
        for _ in 0..10 {
            let interner_clone = Arc::clone(&interner);
            let handle = thread::spawn(move || {
                for i in 0..100 {
                    let path = format!("path_{}.txt", i);
                    interner_clone.intern(&path);
                }
            });
            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // Should have exactly 100 unique paths
        assert_eq!(interner.len(), 100);
    }

    #[test]
    fn test_path_interner_memory_deduplication() {
        let interner = PathInterner::new();

        // Intern the same path 1000 times
        let paths: Vec<_> = (0..1000)
            .map(|_| interner.intern("same/path.txt"))
            .collect();

        // All should point to the same allocation
        for i in 1..paths.len() {
            assert!(Arc::ptr_eq(&paths[0], &paths[i]));
        }

        // Should only have 1 unique path
        assert_eq!(interner.len(), 1);
    }

    #[test]
    fn test_watcher_config_defaults() {
        let config = WatcherConfig::default();

        assert_eq!(config.debounce_duration, Duration::from_millis(300));
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.batch_timeout, Duration::from_millis(100));
        assert_eq!(config.fsevent_latency, Duration::from_millis(50));
        assert!(config.detect_atomic_saves);
        assert_eq!(config.overflow_strategy, OverflowStrategy::PauseAndRescan);
    }

    #[test]
    fn test_watch_event_creation() {
        let interner = PathInterner::new();
        let path = interner.intern("test.txt");

        let event = WatchEvent::new(path.clone(), EventKind::Modify(ModifyKind::Data));

        assert_eq!(event.path.as_ref(), Path::new("test.txt"));
        assert_eq!(event.kind, EventKind::Modify(ModifyKind::Data));
    }

    #[test]
    fn test_event_kind_equality() {
        assert_eq!(EventKind::Create, EventKind::Create);
        assert_eq!(EventKind::Modify(ModifyKind::Data), EventKind::Modify(ModifyKind::Data));
        assert_ne!(EventKind::Modify(ModifyKind::Data), EventKind::Modify(ModifyKind::Metadata));
        assert_ne!(EventKind::Create, EventKind::Delete);
    }

    #[test]
    fn test_watcher_initialization() {
        let temp_dir = std::env::temp_dir();
        let watcher = Watcher::new(&temp_dir).unwrap();

        assert_eq!(watcher.root(), &temp_dir);
        assert_eq!(watcher.interner().len(), 0);
        assert!(!watcher.is_running());
    }

    #[test]
    fn test_watcher_with_custom_config() {
        let temp_dir = std::env::temp_dir();
        let mut config = WatcherConfig::default();
        config.debounce_duration = Duration::from_millis(500);
        config.batch_size = 50;

        let watcher = Watcher::with_config(&temp_dir, config.clone()).unwrap();

        assert_eq!(watcher.config().debounce_duration, Duration::from_millis(500));
        assert_eq!(watcher.config().batch_size, 50);
        assert!(!watcher.is_running());
    }

    #[tokio::test]
    async fn test_watcher_start_stop() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let mut watcher = Watcher::new(temp_dir.path()).unwrap();

        assert!(!watcher.is_running());

        watcher.start().await.unwrap();
        assert!(watcher.is_running());

        watcher.stop().await.unwrap();
        assert!(!watcher.is_running());
    }

    #[tokio::test]
    async fn test_watcher_end_to_end() {
        use tempfile::TempDir;
        use std::fs;

        let temp_dir = TempDir::new().unwrap();
        let mut watcher = Watcher::new(temp_dir.path()).unwrap();

        watcher.start().await.unwrap();

        // Create a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, b"hello world").unwrap();

        // Poll for events
        for _ in 0..10 {
            watcher.poll_events().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Wait for debounce
        tokio::time::sleep(Duration::from_millis(350)).await;

        // Should have debounced events
        let batch = watcher.next_batch();
        assert!(!batch.is_empty(), "Should have received at least one event");

        watcher.stop().await.unwrap();
    }

    #[test]
    fn test_watcher_flush() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let mut watcher = Watcher::new(temp_dir.path()).unwrap();

        // Flush should work even when not started
        let flushed = watcher.flush();
        assert!(flushed.is_empty());
    }
}
