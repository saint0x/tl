//! Event coalescing and deduplication
//!
//! Reduces event noise by:
//! - Deduplicating identical events
//! - Merging event sequences (Create + Modify → Create)
//! - Canceling opposing events (Create + Delete → nothing)
//! - Detecting atomic saves (temp file operations)

use crate::{EventKind, ModifyKind, WatchEvent};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Event coalescer that merges and deduplicates file system events
pub struct Coalescer {
    /// Window duration for coalescing events (default: 100ms)
    window: Duration,

    /// Pending events, keyed by path
    pending: HashMap<PathBuf, CoalescedEvent>,

    /// Detect atomic save patterns
    detect_atomic_saves: bool,
}

/// A coalesced event with timing information
#[derive(Debug, Clone)]
struct CoalescedEvent {
    /// The current event kind after coalescing
    kind: EventKind,

    /// Interned path
    path: Arc<Path>,

    /// When this event was first seen
    first_seen: Instant,

    /// When this event was last updated
    last_updated: Instant,

    /// Number of events coalesced into this one
    count: usize,
}

impl Coalescer {
    /// Create a new coalescer with default settings
    pub fn new() -> Self {
        Self::with_config(Duration::from_millis(100), true)
    }

    /// Create a new coalescer with custom configuration
    pub fn with_config(window: Duration, detect_atomic_saves: bool) -> Self {
        Self {
            window,
            pending: HashMap::new(),
            detect_atomic_saves,
        }
    }

    /// Add an event to the coalescer
    ///
    /// Events are merged with existing events for the same path.
    /// Returns None if the event was merged, or Some if it should be emitted immediately.
    pub fn push(&mut self, event: WatchEvent) -> Option<WatchEvent> {
        let path_buf = event.path.to_path_buf();

        // Check for atomic save patterns
        if self.detect_atomic_saves {
            if let Some(target) = self.detect_atomic_save_pattern(&event) {
                // Convert atomic save to a modify event on the target file
                return Some(WatchEvent::new(
                    Arc::from(target.as_path()),
                    EventKind::Modify(ModifyKind::Data),
                ));
            }
        }

        // Get existing event for this path
        if let Some(existing) = self.pending.get_mut(&path_buf) {
            // Coalesce with existing event
            let coalesced_kind = Self::coalesce_kinds(existing.kind, event.kind);

            // If coalescing results in no event (e.g., Create + Delete), remove it
            if let Some(kind) = coalesced_kind {
                existing.kind = kind;
                existing.last_updated = Instant::now();
                existing.count += 1;
                None
            } else {
                // Events cancel out - remove from pending
                self.pending.remove(&path_buf);
                None
            }
        } else {
            // New event for this path
            self.pending.insert(
                path_buf,
                CoalescedEvent {
                    kind: event.kind,
                    path: event.path.clone(),
                    first_seen: Instant::now(),
                    last_updated: Instant::now(),
                    count: 1,
                },
            );
            None
        }
    }

    /// Coalesce two event kinds
    ///
    /// Returns None if events cancel out, or Some(kind) for the coalesced event.
    fn coalesce_kinds(existing: EventKind, new: EventKind) -> Option<EventKind> {
        use EventKind::*;

        match (existing, new) {
            // Create + anything = Create (file didn't exist before)
            (Create, Create) => Some(Create),
            (Create, Modify(_)) => Some(Create),
            (Create, Delete) => None, // Create + Delete cancels out

            // Modify + Modify = Modify
            (Modify(_), Modify(new_kind)) => Some(Modify(new_kind)),
            (Modify(_), Delete) => Some(Delete),
            (Modify(_), Create) => Some(Modify(ModifyKind::Data)), // Unlikely but handle it

            // Delete + Create = Modify (file replaced)
            (Delete, Create) => Some(Modify(ModifyKind::Data)),
            (Delete, Modify(_)) => Some(Modify(ModifyKind::Data)), // Unlikely
            (Delete, Delete) => Some(Delete),

            // Rename events
            (Rename, _) => Some(new),
            (_, Rename) => Some(Rename),

            // Overflow events always propagate
            (Overflow, _) | (_, Overflow) => Some(Overflow),
        }
    }

    /// Collect events that are ready to emit
    ///
    /// Returns events that haven't been updated within the coalescing window.
    pub fn collect_ready(&mut self) -> Vec<WatchEvent> {
        let now = Instant::now();
        let mut ready = Vec::new();
        let mut to_remove = Vec::new();

        for (path, event) in &self.pending {
            if now.duration_since(event.last_updated) >= self.window {
                ready.push(WatchEvent::new(event.path.clone(), event.kind));
                to_remove.push(path.clone());
            }
        }

        // Remove emitted events
        for path in to_remove {
            self.pending.remove(&path);
        }

        ready
    }

    /// Flush all pending events
    ///
    /// Emits all pending events regardless of the coalescing window.
    /// Useful for shutdown or forced checkpointing.
    pub fn flush(&mut self) -> Vec<WatchEvent> {
        let events: Vec<_> = self
            .pending
            .values()
            .map(|e| WatchEvent::new(e.path.clone(), e.kind))
            .collect();

        self.pending.clear();
        events
    }

    /// Get the number of pending events
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Detect atomic save patterns
    ///
    /// Returns the target file path if this is an atomic save temporary file.
    fn detect_atomic_save_pattern(&self, event: &WatchEvent) -> Option<PathBuf> {
        let path = event.path.as_ref();
        let filename = path.file_name()?.to_str()?;

        // Vim swap files: .file.swp → file
        if filename.starts_with('.') && filename.ends_with(".swp") {
            let target = filename.strip_prefix('.')?.strip_suffix(".swp")?;
            return Some(path.with_file_name(target));
        }

        // TextEdit temp files: .file.tmp → file
        if filename.starts_with('.') && filename.contains(".tmp") {
            let target = filename.strip_prefix('.')?.split(".tmp").next()?;
            return Some(path.with_file_name(target));
        }

        // Generic temp pattern: file.tmp → file
        if filename.ends_with(".tmp") {
            let target = filename.strip_suffix(".tmp")?;
            return Some(path.with_file_name(target));
        }

        // Emacs backup: file~ → file
        if filename.ends_with('~') {
            let target = filename.strip_suffix('~')?;
            return Some(path.with_file_name(target));
        }

        None
    }
}

impl Default for Coalescer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(path: &str, kind: EventKind) -> WatchEvent {
        WatchEvent::new(Arc::from(Path::new(path)), kind)
    }

    #[test]
    fn test_deduplicate_modify_events() {
        let mut coalescer = Coalescer::new();

        // Multiple modify events for same file
        coalescer.push(make_event("foo.txt", EventKind::Modify(ModifyKind::Data)));
        coalescer.push(make_event("foo.txt", EventKind::Modify(ModifyKind::Data)));
        coalescer.push(make_event("foo.txt", EventKind::Modify(ModifyKind::Data)));

        // Should have only 1 pending event
        assert_eq!(coalescer.pending_count(), 1);

        // Flush should return 1 event
        let events = coalescer.flush();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Modify(ModifyKind::Data));
    }

    #[test]
    fn test_create_delete_cancellation() {
        let mut coalescer = Coalescer::new();

        coalescer.push(make_event("temp.txt", EventKind::Create));
        coalescer.push(make_event("temp.txt", EventKind::Delete));

        // Events should cancel out
        assert_eq!(coalescer.pending_count(), 0);
    }

    #[test]
    fn test_delete_create_becomes_modify() {
        let mut coalescer = Coalescer::new();

        coalescer.push(make_event("file.txt", EventKind::Delete));
        coalescer.push(make_event("file.txt", EventKind::Create));

        // Should coalesce to Modify (file replaced)
        let events = coalescer.flush();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Modify(ModifyKind::Data));
    }

    #[test]
    fn test_create_modify_stays_create() {
        let mut coalescer = Coalescer::new();

        coalescer.push(make_event("new.txt", EventKind::Create));
        coalescer.push(make_event("new.txt", EventKind::Modify(ModifyKind::Data)));

        // Should stay as Create
        let events = coalescer.flush();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Create);
    }

    #[test]
    fn test_multiple_files_independent() {
        let mut coalescer = Coalescer::new();

        coalescer.push(make_event("a.txt", EventKind::Modify(ModifyKind::Data)));
        coalescer.push(make_event("b.txt", EventKind::Modify(ModifyKind::Data)));
        coalescer.push(make_event("c.txt", EventKind::Create));

        // Should have 3 independent pending events
        assert_eq!(coalescer.pending_count(), 3);
    }

    #[test]
    fn test_atomic_save_vim_swp() {
        let coalescer = Coalescer::new();

        let event = make_event("dir/.file.txt.swp", EventKind::Create);
        let target = coalescer.detect_atomic_save_pattern(&event);

        assert_eq!(target, Some(PathBuf::from("dir/file.txt")));
    }

    #[test]
    fn test_atomic_save_tmp_file() {
        let coalescer = Coalescer::new();

        let event = make_event("file.txt.tmp", EventKind::Modify(ModifyKind::Data));
        let target = coalescer.detect_atomic_save_pattern(&event);

        assert_eq!(target, Some(PathBuf::from("file.txt")));
    }

    #[test]
    fn test_atomic_save_emacs_backup() {
        let coalescer = Coalescer::new();

        let event = make_event("file.txt~", EventKind::Modify(ModifyKind::Data));
        let target = coalescer.detect_atomic_save_pattern(&event);

        assert_eq!(target, Some(PathBuf::from("file.txt")));
    }

    #[test]
    fn test_atomic_save_push_converts_to_modify() {
        let mut coalescer = Coalescer::new();

        let result = coalescer.push(make_event(".file.txt.swp", EventKind::Create));

        // Should return a Modify event for file.txt
        assert!(result.is_some());
        let event = result.unwrap();
        assert_eq!(event.path.to_str().unwrap(), "file.txt");
        assert_eq!(event.kind, EventKind::Modify(ModifyKind::Data));
    }

    #[test]
    fn test_collect_ready_respects_window() {
        let mut coalescer = Coalescer::with_config(Duration::from_millis(50), false);

        coalescer.push(make_event("a.txt", EventKind::Modify(ModifyKind::Data)));

        // Immediately after push, nothing should be ready
        let ready = coalescer.collect_ready();
        assert_eq!(ready.len(), 0);

        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(60));

        // Now event should be ready
        let ready = coalescer.collect_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(coalescer.pending_count(), 0);
    }

    #[test]
    fn test_flush_returns_all_events() {
        let mut coalescer = Coalescer::new();

        coalescer.push(make_event("a.txt", EventKind::Modify(ModifyKind::Data)));
        coalescer.push(make_event("b.txt", EventKind::Create));
        coalescer.push(make_event("c.txt", EventKind::Delete));

        let events = coalescer.flush();
        assert_eq!(events.len(), 3);
        assert_eq!(coalescer.pending_count(), 0);
    }

    #[test]
    fn test_overflow_events_always_propagate() {
        let mut coalescer = Coalescer::new();

        coalescer.push(make_event("foo.txt", EventKind::Modify(ModifyKind::Data)));
        coalescer.push(make_event("foo.txt", EventKind::Overflow));

        let events = coalescer.flush();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Overflow);
    }
}
