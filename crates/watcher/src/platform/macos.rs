//! macOS FSEvents implementation
//!
//! Uses FSEvents (via notify crate) for file system monitoring on macOS.
//! FSEvents provides efficient, kernel-level file system monitoring with
//! coarse-grained events (directory-level by default).

use super::{PlatformWatcher, WatcherDiagnostics};
use crate::{EventKind, ModifyKind, WatchEvent, WatcherConfig};
use anyhow::{Context, Result};
use async_trait::async_trait;
use crossbeam_channel::{Receiver, Sender};
use notify::{Config, Event, EventKind as NotifyEventKind, RecommendedWatcher, Watcher as NotifyWatcher, RecursiveMode};
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// macOS-specific watcher using FSEvents
pub struct MacOSWatcher {
    /// Root path being watched
    root: PathBuf,

    /// Underlying notify watcher
    watcher: Option<RecommendedWatcher>,

    /// Event receiver channel
    event_rx: Receiver<notify::Result<Event>>,

    /// Event sender channel (kept for watcher initialization)
    event_tx: Sender<notify::Result<Event>>,

    /// Diagnostics (protected by RwLock for interior mutability)
    diagnostics: Arc<RwLock<WatcherDiagnostics>>,

    /// Overflow detected flag
    overflow_detected: bool,

    /// Running state
    is_running: bool,

    /// Configuration
    config: WatcherConfig,
}

impl MacOSWatcher {
    /// Create a new macOS watcher
    pub fn new(path: &Path, config: &WatcherConfig) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();

        Ok(Self {
            root: path.to_path_buf(),
            watcher: None,
            event_rx: rx,
            event_tx: tx,
            diagnostics: Arc::new(RwLock::new(WatcherDiagnostics::default())),
            overflow_detected: false,
            is_running: false,
            config: config.clone(),
        })
    }

    /// Convert notify::Event to WatchEvent
    fn convert_event(&mut self, event: Event) -> Option<WatchEvent> {
        // Update diagnostics
        {
            let mut diag = self.diagnostics.write();
            diag.events_received += 1;

            // Check for FSEvents overflow flags
            // FSEvents doesn't have explicit overflow events, but we can detect
            // them through event flags or queue depth heuristics
            if matches!(event.kind, NotifyEventKind::Other) {
                diag.overflow_count += 1;
            }
        }

        // Get the first path from the event (FSEvents usually has one path per event)
        let path = event.paths.first()?;

        // Filter system files
        if self.should_filter(path) {
            let mut diag = self.diagnostics.write();
            diag.events_filtered += 1;
            return None;
        }

        // Make path relative to root
        let relative_path = if path.starts_with(&self.root) {
            path.strip_prefix(&self.root).ok()?.to_path_buf()
        } else {
            path.to_path_buf()
        };

        // Convert event kind
        let kind = match event.kind {
            NotifyEventKind::Create(_) => EventKind::Create,
            NotifyEventKind::Modify(notify::event::ModifyKind::Data(_)) => {
                EventKind::Modify(ModifyKind::Data)
            }
            NotifyEventKind::Modify(notify::event::ModifyKind::Metadata(_)) => {
                EventKind::Modify(ModifyKind::Metadata)
            }
            NotifyEventKind::Modify(_) => EventKind::Modify(ModifyKind::Any),
            NotifyEventKind::Remove(_) => EventKind::Delete,
            NotifyEventKind::Other => {
                // FSEvents sometimes reports file creations/truncations as "Other"
                // (notably for empty files). Treat as a generic modification so we
                // don't miss real changes.
                EventKind::Modify(ModifyKind::Any)
            }
            _ => EventKind::Modify(ModifyKind::Any),
        };

        Some(WatchEvent::new(Arc::from(relative_path.as_path()), kind))
    }

    /// Check if a path should be filtered
    fn should_filter(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        // Filter macOS system files
        if path_str.contains(".DS_Store") {
            return true;
        }

        // Filter .tl directory
        if path_str.contains("/.tl/") || path.ends_with(".tl") {
            return true;
        }

        // Filter .git directory
        if path_str.contains("/.git/") || path.ends_with(".git") {
            return true;
        }

        // Detect atomic save patterns (Vim, TextEdit, etc.)
        if self.config.detect_atomic_saves && self.is_atomic_save_temp(path) {
            let mut diag = self.diagnostics.write();
            diag.atomic_saves_detected += 1;
            return true;
        }

        false
    }

    /// Check if path is an atomic save temporary file
    fn is_atomic_save_temp(&self, path: &Path) -> bool {
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");

        // Vim swap files
        if filename.starts_with('.') && (filename.ends_with(".swp") || filename.ends_with(".swo")) {
            return true;
        }

        // TextEdit temporary files
        if filename.starts_with('.') && filename.contains(".tmp") {
            return true;
        }

        // Emacs backup files
        if filename.ends_with('~') || filename.starts_with('#') {
            return true;
        }

        false
    }
}

#[async_trait]
impl PlatformWatcher for MacOSWatcher {
    async fn start(&mut self) -> Result<()> {
        if self.is_running {
            return Ok(());
        }

        let tx = self.event_tx.clone();

        // Create notify watcher with FSEvents configuration
        let notify_config = Config::default()
            .with_poll_interval(self.config.fsevent_latency)
            .with_compare_contents(false); // FSEvents doesn't support content comparison

        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            notify_config,
        )
        .context("Failed to create FSEvents watcher")?;

        self.watcher = Some(watcher);

        // Start watching the root path recursively
        self.watcher
            .as_mut()
            .unwrap()
            .watch(&self.root, RecursiveMode::Recursive)
            .context("Failed to start watching directory")?;

        self.is_running = true;

        {
            let mut diag = self.diagnostics.write();
            diag.platform_info = format!("FSEvents watching: {}", self.root.display());
        }

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }

        if let Some(mut watcher) = self.watcher.take() {
            watcher
                .unwatch(&self.root)
                .context("Failed to unwatch directory")?;
        }

        self.is_running = false;
        Ok(())
    }

    async fn poll_event(&mut self) -> Result<Option<WatchEvent>> {
        // Non-blocking receive
        match self.event_rx.try_recv() {
            Ok(Ok(event)) => Ok(self.convert_event(event)),
            Ok(Err(e)) => {
                // Error from notify - might indicate overflow or other issues
                self.overflow_detected = true;
                let mut diag = self.diagnostics.write();
                diag.overflow_count += 1;
                Err(e.into())
            }
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                anyhow::bail!("Event channel disconnected")
            }
        }
    }

    fn has_overflow(&self) -> bool {
        self.overflow_detected
    }

    fn reset_overflow(&mut self) {
        self.overflow_detected = false;
    }

    fn diagnostics(&self) -> WatcherDiagnostics {
        self.diagnostics.read().clone()
    }

    fn is_running(&self) -> bool {
        self.is_running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_macos_watcher_initialization() {
        let temp_dir = TempDir::new().unwrap();
        let config = WatcherConfig::default();

        let watcher = MacOSWatcher::new(temp_dir.path(), &config).unwrap();

        assert!(!watcher.is_running());
        assert!(!watcher.has_overflow());
        assert_eq!(watcher.diagnostics().events_received, 0);
    }

    #[tokio::test]
    async fn test_macos_watcher_start_stop() {
        let temp_dir = TempDir::new().unwrap();
        let config = WatcherConfig::default();

        let mut watcher = MacOSWatcher::new(temp_dir.path(), &config).unwrap();

        watcher.start().await.unwrap();
        assert!(watcher.is_running());

        watcher.stop().await.unwrap();
        assert!(!watcher.is_running());
    }

    #[tokio::test]
    async fn test_macos_watcher_file_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config = WatcherConfig::default();

        let mut watcher = MacOSWatcher::new(temp_dir.path(), &config).unwrap();
        watcher.start().await.unwrap();

        // Create a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, b"hello").unwrap();

        // Give FSEvents time to process
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Poll for events
        let mut found_event = false;
        for _ in 0..10 {
            if let Some(event) = watcher.poll_event().await.unwrap() {
                if event.path.to_string_lossy().contains("test.txt") {
                    found_event = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(found_event, "Should have detected file creation");
        watcher.stop().await.unwrap();
    }

    #[test]
    fn test_should_filter_system_files() {
        let temp_dir = TempDir::new().unwrap();
        let config = WatcherConfig::default();
        let watcher = MacOSWatcher::new(temp_dir.path(), &config).unwrap();

        assert!(watcher.should_filter(Path::new("/foo/.DS_Store")));
        assert!(watcher.should_filter(Path::new("/foo/.git/config")));
        assert!(watcher.should_filter(Path::new("/foo/.tl/store.db")));
        assert!(!watcher.should_filter(Path::new("/foo/bar.txt")));
    }

    #[test]
    fn test_atomic_save_detection() {
        let temp_dir = TempDir::new().unwrap();
        let config = WatcherConfig::default();
        let watcher = MacOSWatcher::new(temp_dir.path(), &config).unwrap();

        // Vim swap files
        assert!(watcher.is_atomic_save_temp(Path::new(".file.swp")));
        assert!(watcher.is_atomic_save_temp(Path::new(".file.swo")));

        // Emacs backup files
        assert!(watcher.is_atomic_save_temp(Path::new("file~")));
        assert!(watcher.is_atomic_save_temp(Path::new("#file#")));

        // TextEdit temp files
        assert!(watcher.is_atomic_save_temp(Path::new(".file.tmp")));

        // Normal files
        assert!(!watcher.is_atomic_save_temp(Path::new("file.txt")));
        assert!(!watcher.is_atomic_save_temp(Path::new("file.rs")));
    }
}
