//! Platform-specific file watching implementations
//!
//! Provides a unified interface (PlatformWatcher trait) with platform-specific
//! implementations for macOS (FSEvents) and Linux (inotify).

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

use crate::WatchEvent;
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

#[cfg(target_os = "macos")]
pub use macos::MacOSWatcher;

#[cfg(target_os = "linux")]
pub use linux::LinuxWatcher;

/// Platform-specific watcher diagnostics
#[derive(Debug, Clone, Default)]
pub struct WatcherDiagnostics {
    /// Total events received from OS
    pub events_received: u64,

    /// Events filtered (ignored paths, etc)
    pub events_filtered: u64,

    /// Number of overflow events detected
    pub overflow_count: u64,

    /// Atomic save patterns detected
    pub atomic_saves_detected: u64,

    /// Platform-specific info (e.g., "FSEvents queue depth: 42")
    pub platform_info: String,
}

/// Platform-agnostic file system watcher interface
///
/// Abstracts platform-specific implementations (FSEvents on macOS, inotify on Linux)
/// with a unified async interface.
#[async_trait]
pub trait PlatformWatcher: Send + Sync {
    /// Start watching the configured path
    ///
    /// Begins monitoring the file system for changes. Events are queued
    /// and can be retrieved via poll_event().
    async fn start(&mut self) -> Result<()>;

    /// Stop watching
    ///
    /// Stops monitoring and cleans up resources. After stopping, no new
    /// events will be queued.
    async fn stop(&mut self) -> Result<()>;

    /// Poll for the next event (non-blocking)
    ///
    /// Returns None if no events are currently available.
    /// This is the main event retrieval interface.
    async fn poll_event(&mut self) -> Result<Option<WatchEvent>>;

    /// Check if an overflow condition has been detected
    ///
    /// Overflow occurs when the OS event buffer fills up and events are lost.
    /// Different platforms detect this differently:
    /// - macOS FSEvents: Check flags on events
    /// - Linux inotify: IN_Q_OVERFLOW event
    fn has_overflow(&self) -> bool;

    /// Reset the overflow flag
    ///
    /// Called after overflow recovery has been performed.
    fn reset_overflow(&mut self);

    /// Get diagnostic information
    ///
    /// Returns statistics about watcher performance and state.
    fn diagnostics(&self) -> WatcherDiagnostics;

    /// Check if the watcher is currently active
    fn is_running(&self) -> bool;
}

/// Create a platform-specific watcher instance
///
/// This is a factory function that returns the appropriate watcher type
/// for the current platform.
#[cfg(target_os = "macos")]
pub fn create_platform_watcher(
    path: &Path,
    config: &crate::WatcherConfig,
) -> Result<Box<dyn PlatformWatcher>> {
    Ok(Box::new(MacOSWatcher::new(path, config)?))
}

#[cfg(target_os = "linux")]
pub fn create_platform_watcher(
    path: &Path,
    config: &crate::WatcherConfig,
) -> Result<Box<dyn PlatformWatcher>> {
    Ok(Box::new(LinuxWatcher::new(path, config)?))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn create_platform_watcher(
    _path: &Path,
    _config: &crate::WatcherConfig,
) -> Result<Box<dyn PlatformWatcher>> {
    anyhow::bail!("Unsupported platform - only macOS and Linux are supported")
}
