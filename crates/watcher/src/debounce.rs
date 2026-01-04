//! Per-path debouncing with async timers
//!
//! Prevents excessive checkpoint creation by:
//! - Tracking per-path timers (300ms default)
//! - Resetting timers on repeated events
//! - Batching emissions for efficiency
//!
//! Example: If a file is modified 100 times in 1 second, only one debounced
//! event is emitted 300ms after the last modification.

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;

/// Per-path debouncer with async timers
pub struct Debouncer {
    /// Per-path debounce state
    state: Arc<DashMap<PathBuf, DebounceEntry>>,

    /// Debounce duration (how long to wait after last event)
    debounce_duration: Duration,

    /// Batch size for emission (emit when this many paths are ready)
    batch_size: usize,

    /// Batch timeout (emit after this duration even if batch not full)
    batch_timeout: Duration,

    /// Channel for debounced paths
    tx: mpsc::UnboundedSender<Arc<Path>>,

    /// Receiver for debounced paths
    rx: mpsc::UnboundedReceiver<Arc<Path>>,
}

/// Per-path debounce state
#[derive(Debug)]
struct DebounceEntry {
    /// Path being debounced (interned)
    path: Arc<Path>,

    /// Last time an event was received for this path
    last_event: Instant,

    /// When the timer is scheduled to fire
    scheduled_fire: Instant,
}

impl Debouncer {
    /// Create a new debouncer with default settings
    pub fn new() -> Self {
        Self::with_config(
            Duration::from_millis(300),
            100,
            Duration::from_millis(100),
        )
    }

    /// Create a new debouncer with custom configuration
    pub fn with_config(
        debounce_duration: Duration,
        batch_size: usize,
        batch_timeout: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        Self {
            state: Arc::new(DashMap::new()),
            debounce_duration,
            batch_size,
            batch_timeout,
            tx,
            rx,
        }
    }

    /// Register an event for debouncing
    ///
    /// Starts or resets the debounce timer for the given path.
    /// The path will be emitted after debounce_duration has elapsed
    /// with no further events.
    pub fn push(&self, path: Arc<Path>) {
        let path_buf = path.to_path_buf();
        let now = Instant::now();
        let scheduled_fire = now + self.debounce_duration;

        // Check if we already have a timer for this path
        if let Some(mut entry) = self.state.get_mut(&path_buf) {
            // Update timing - this effectively "resets" the timer
            entry.last_event = now;
            entry.scheduled_fire = scheduled_fire;
        } else {
            // New path - create entry and spawn timer task
            self.state.insert(
                path_buf.clone(),
                DebounceEntry {
                    path: path.clone(),
                    last_event: now,
                    scheduled_fire,
                },
            );

            // Spawn async timer task
            let state = Arc::clone(&self.state);
            let tx = self.tx.clone();
            let debounce_duration = self.debounce_duration;

            tokio::spawn(async move {
                // Wait for debounce duration
                sleep(debounce_duration).await;

                // Check if event should fire
                if let Some((_, entry)) = state.remove(&path_buf) {
                    let now = Instant::now();

                    // Only emit if no new events came in during our sleep
                    if now >= entry.scheduled_fire {
                        // Timer expired cleanly - emit path
                        let _ = tx.send(entry.path);
                    } else {
                        // Event was updated - re-insert and spawn new timer
                        let remaining = entry.scheduled_fire.duration_since(now);
                        state.insert(path_buf.clone(), entry);

                        // Spawn new timer for remaining duration
                        let state = state.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            sleep(remaining).await;

                            if let Some((_, entry)) = state.remove(&path_buf) {
                                let _ = tx.send(entry.path);
                            }
                        });
                    }
                }
            });
        }
    }

    /// Try to receive a batch of debounced paths (non-blocking)
    ///
    /// Returns immediately with whatever paths are currently available.
    pub fn try_recv_batch(&mut self) -> Vec<Arc<Path>> {
        let mut batch = Vec::new();

        // Drain up to batch_size paths
        while batch.len() < self.batch_size {
            match self.rx.try_recv() {
                Ok(path) => batch.push(path),
                Err(_) => break,
            }
        }

        batch
    }

    /// Receive a batch of debounced paths (async, with timeout)
    ///
    /// Waits up to batch_timeout for batch_size paths to arrive.
    /// Returns earlier if batch_size paths are ready.
    pub async fn recv_batch(&mut self) -> Vec<Arc<Path>> {
        let mut batch = Vec::new();
        let deadline = tokio::time::Instant::now() + self.batch_timeout;

        loop {
            // Try to fill the batch
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(path)) => {
                    batch.push(path);

                    // Check if batch is full
                    if batch.len() >= self.batch_size {
                        break;
                    }
                }
                Ok(None) => {
                    // Channel closed
                    break;
                }
                Err(_) => {
                    // Timeout expired - return whatever we have
                    break;
                }
            }
        }

        batch
    }

    /// Get the number of paths currently being debounced
    pub fn pending_count(&self) -> usize {
        self.state.len()
    }

    /// Flush all pending debounces immediately
    ///
    /// Forces emission of all paths currently in debounce state.
    /// Useful for shutdown or forced checkpointing.
    pub fn flush(&mut self) -> Vec<Arc<Path>> {
        let mut paths = Vec::new();

        // Extract all pending paths
        for item in self.state.iter() {
            paths.push(item.value().path.clone());
        }

        // Clear state
        self.state.clear();

        paths
    }
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_debouncer_basic() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_millis(50),
            10,
            Duration::from_millis(100),
        );

        let path: Arc<Path> = Arc::from(Path::new("test.txt"));
        debouncer.push(path.clone());

        // Wait for debounce to complete
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Should receive the debounced path
        let batch = debouncer.try_recv_batch();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].as_ref(), Path::new("test.txt"));
    }

    #[tokio::test]
    async fn test_debouncer_reset_on_new_event() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_millis(100),
            10,
            Duration::from_millis(200),
        );

        let path: Arc<Path> = Arc::from(Path::new("file.txt"));

        // Send initial event
        debouncer.push(path.clone());

        // Wait 50ms, then send another event (should reset timer)
        tokio::time::sleep(Duration::from_millis(50)).await;
        debouncer.push(path.clone());

        // Wait another 50ms (total 100ms from initial, but only 50ms from second event)
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Should NOT have fired yet (timer was reset)
        let batch = debouncer.try_recv_batch();
        assert_eq!(batch.len(), 0);

        // Wait for full debounce from second event
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Now should have fired
        let batch = debouncer.try_recv_batch();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn test_debouncer_multiple_paths_independent() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_millis(50),
            10,
            Duration::from_millis(100),
        );

        let path1: Arc<Path> = Arc::from(Path::new("a.txt"));
        let path2: Arc<Path> = Arc::from(Path::new("b.txt"));
        let path3: Arc<Path> = Arc::from(Path::new("c.txt"));

        debouncer.push(path1);
        debouncer.push(path2);
        debouncer.push(path3);

        // Wait for debounces
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Should receive all 3 paths
        let batch = debouncer.try_recv_batch();
        assert_eq!(batch.len(), 3);
    }

    #[tokio::test]
    async fn test_debouncer_batch_size_limit() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_millis(10),
            5, // Small batch size
            Duration::from_millis(100),
        );

        // Push 10 paths
        for i in 0..10 {
            let path: Arc<Path> = Arc::from(Path::new(&format!("file_{}.txt", i)));
            debouncer.push(path);
        }

        // Wait for debounces
        tokio::time::sleep(Duration::from_millis(20)).await;

        // First batch should have 5 paths (batch size limit)
        let batch1 = debouncer.try_recv_batch();
        assert_eq!(batch1.len(), 5);

        // Second batch should have remaining 5
        let batch2 = debouncer.try_recv_batch();
        assert_eq!(batch2.len(), 5);
    }

    #[tokio::test]
    async fn test_debouncer_pending_count() {
        let debouncer = Debouncer::with_config(
            Duration::from_millis(100),
            10,
            Duration::from_millis(100),
        );

        assert_eq!(debouncer.pending_count(), 0);

        debouncer.push(Arc::from(Path::new("a.txt")));
        debouncer.push(Arc::from(Path::new("b.txt")));

        // Should have 2 pending
        assert_eq!(debouncer.pending_count(), 2);

        // Wait for debounces to complete
        tokio::time::sleep(Duration::from_millis(110)).await;

        // Should have 0 pending (timers fired)
        assert_eq!(debouncer.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_debouncer_flush() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_secs(10), // Very long debounce
            10,
            Duration::from_millis(100),
        );

        debouncer.push(Arc::from(Path::new("a.txt")));
        debouncer.push(Arc::from(Path::new("b.txt")));
        debouncer.push(Arc::from(Path::new("c.txt")));

        // Flush immediately without waiting for debounce
        let paths = debouncer.flush();

        assert_eq!(paths.len(), 3);
        assert_eq!(debouncer.pending_count(), 0);
    }

    #[tokio::test]
    async fn test_debouncer_rapid_updates() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_millis(100), // Longer debounce
            10,
            Duration::from_millis(100),
        );

        let path: Arc<Path> = Arc::from(Path::new("rapidly_changing.txt"));

        // Simulate rapid file modifications (30ms total)
        for _ in 0..30 {
            debouncer.push(path.clone());
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Wait for final debounce
        tokio::time::sleep(Duration::from_millis(110)).await;

        // Should receive very few events (ideally 1) despite 30 modifications
        let batch = debouncer.try_recv_batch();
        assert!(batch.len() <= 2, "Expected at most 2 events, got {}", batch.len());
        assert_eq!(batch[0].as_ref(), Path::new("rapidly_changing.txt"));
    }

    #[tokio::test]
    async fn test_debouncer_recv_batch_timeout() {
        let mut debouncer = Debouncer::with_config(
            Duration::from_millis(10),
            100, // Large batch size (won't be reached)
            Duration::from_millis(50), // Short batch timeout
        );

        debouncer.push(Arc::from(Path::new("a.txt")));
        debouncer.push(Arc::from(Path::new("b.txt")));

        // Wait for debounces
        tokio::time::sleep(Duration::from_millis(20)).await;

        // recv_batch should return after batch_timeout even though batch isn't full
        let start = Instant::now();
        let batch = debouncer.recv_batch().await;
        let elapsed = start.elapsed();

        assert_eq!(batch.len(), 2);
        assert!(elapsed < Duration::from_millis(100)); // Should return quickly
    }
}
