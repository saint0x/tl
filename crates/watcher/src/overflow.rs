//! Overflow recovery with targeted rescan
//!
//! When the OS event buffer overflows, events may be lost. This module
//! provides intelligent recovery by:
//! - Detecting overflow conditions from platform watchers
//! - Performing targeted rescans based on mtime heuristics (NOT full repo scans)
//! - Finding files modified since last checkpoint
//! - Minimizing performance impact during recovery

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use walkdir::WalkDir;

/// Overflow recovery coordinator
pub struct OverflowRecovery {
    /// Root directory being watched
    root: PathBuf,

    /// Last successful checkpoint time
    /// Files modified after this time are candidates for rescan
    last_checkpoint: Option<SystemTime>,
}

impl OverflowRecovery {
    /// Create a new overflow recovery instance
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            last_checkpoint: None,
        }
    }

    /// Update the last checkpoint time
    ///
    /// Call this after each successful checkpoint to track what's been saved.
    pub fn mark_checkpoint(&mut self, time: SystemTime) {
        self.last_checkpoint = Some(time);
    }

    /// Perform targeted overflow recovery
    ///
    /// Returns paths that were modified since the last checkpoint.
    /// Uses mtime-based heuristics to avoid full repository scans.
    pub fn recover(&self) -> Result<Vec<Arc<Path>>> {
        let checkpoint_time = self
            .last_checkpoint
            .unwrap_or_else(|| SystemTime::UNIX_EPOCH);

        // Find suspicious directories (modified since checkpoint)
        let suspicious_dirs = self.find_suspicious_directories(checkpoint_time)?;

        // Scan only suspicious directories for modified files
        let mut modified_files = Vec::new();

        for dir in suspicious_dirs {
            modified_files.extend(self.scan_directory(&dir, checkpoint_time)?);
        }

        Ok(modified_files)
    }

    /// Find directories with mtime > last_checkpoint
    ///
    /// These directories potentially contain modified files.
    /// This is much faster than scanning every file in the repository.
    fn find_suspicious_directories(&self, since: SystemTime) -> Result<Vec<PathBuf>> {
        let mut suspicious = Vec::new();

        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e.path()))
        {
            let entry = entry.context("Failed to read directory entry")?;

            if entry.file_type().is_dir() {
                // Check directory mtime
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if modified > since {
                            suspicious.push(entry.path().to_path_buf());
                        }
                    }
                }
            }
        }

        Ok(suspicious)
    }

    /// Scan a specific directory for files modified since checkpoint
    fn scan_directory(&self, dir: &Path, since: SystemTime) -> Result<Vec<Arc<Path>>> {
        let mut modified = Vec::new();

        for entry in WalkDir::new(dir)
            .max_depth(1) // Only scan immediate children
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e.path()))
        {
            let entry = entry.context("Failed to read directory entry")?;

            // Skip directories (we only want files)
            if entry.file_type().is_dir() {
                continue;
            }

            // Check file mtime
            if let Ok(metadata) = entry.metadata() {
                if let Ok(modified_time) = metadata.modified() {
                    if modified_time > since {
                        // Make path relative to root
                        if let Ok(relative) = entry.path().strip_prefix(&self.root) {
                            modified.push(Arc::from(relative));
                        }
                    }
                }
            }
        }

        Ok(modified)
    }

    /// Check if a path should be ignored during recovery
    fn should_ignore(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        // Ignore .tl directory
        if path_str.contains("/.tl/") || path.ends_with(".tl") {
            return true;
        }

        // Ignore .git directory
        if path_str.contains("/.git/") || path.ends_with(".git") {
            return true;
        }

        // Ignore common build/cache directories
        if path_str.contains("/target/")
            || path_str.contains("/node_modules/")
            || path_str.contains("/.cache/")
            || path_str.contains("/build/")
        {
            return true;
        }

        false
    }

    /// Get estimated number of files that would be scanned during recovery
    ///
    /// Useful for progress reporting or deciding recovery strategy.
    pub fn estimate_scan_size(&self) -> Result<usize> {
        let checkpoint_time = self
            .last_checkpoint
            .unwrap_or_else(|| SystemTime::UNIX_EPOCH);

        let suspicious_dirs = self.find_suspicious_directories(checkpoint_time)?;

        let mut total = 0;
        for dir in suspicious_dirs {
            let count = WalkDir::new(&dir)
                .max_depth(1)
                .into_iter()
                .filter_entry(|e| !self.should_ignore(e.path()))
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .count();
            total += count;
        }

        Ok(total)
    }

    /// Force full repository scan (EMERGENCY ONLY)
    ///
    /// This should be used ONLY as a last resort when mtime heuristics fail.
    /// Prefer targeted recovery whenever possible.
    #[allow(dead_code)]
    fn full_scan(&self) -> Result<Vec<Arc<Path>>> {
        let mut all_files = Vec::new();

        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e.path()))
        {
            let entry = entry.context("Failed to read directory entry")?;

            if entry.file_type().is_file() {
                if let Ok(relative) = entry.path().strip_prefix(&self.root) {
                    all_files.push(Arc::from(relative));
                }
            }
        }

        Ok(all_files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread::sleep;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn test_overflow_recovery_empty_repo() {
        let temp_dir = TempDir::new().unwrap();
        let recovery = OverflowRecovery::new(temp_dir.path());

        let modified = recovery.recover().unwrap();
        assert!(modified.is_empty());
    }

    #[test]
    fn test_overflow_recovery_finds_new_files() {
        let temp_dir = TempDir::new().unwrap();
        let mut recovery = OverflowRecovery::new(temp_dir.path());

        // Mark initial checkpoint
        recovery.mark_checkpoint(SystemTime::now());

        // Sleep to ensure file mtime is after checkpoint
        sleep(Duration::from_millis(10));

        // Create new files
        fs::write(temp_dir.path().join("new1.txt"), b"content").unwrap();
        fs::write(temp_dir.path().join("new2.txt"), b"content").unwrap();

        // Recover should find both files
        let modified = recovery.recover().unwrap();
        assert_eq!(modified.len(), 2);
    }

    #[test]
    fn test_overflow_recovery_ignores_old_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create files BEFORE checkpoint
        fs::write(temp_dir.path().join("old1.txt"), b"content").unwrap();
        fs::write(temp_dir.path().join("old2.txt"), b"content").unwrap();

        sleep(Duration::from_millis(10));

        let mut recovery = OverflowRecovery::new(temp_dir.path());
        recovery.mark_checkpoint(SystemTime::now());

        sleep(Duration::from_millis(10));

        // Create files AFTER checkpoint
        fs::write(temp_dir.path().join("new.txt"), b"content").unwrap();

        // Recover should only find new file
        let modified = recovery.recover().unwrap();
        assert_eq!(modified.len(), 1);
        assert!(modified[0].to_str().unwrap().contains("new.txt"));
    }

    #[test]
    fn test_overflow_recovery_nested_directories() {
        let temp_dir = TempDir::new().unwrap();
        let mut recovery = OverflowRecovery::new(temp_dir.path());

        recovery.mark_checkpoint(SystemTime::now());
        sleep(Duration::from_millis(10));

        // Create nested structure
        let nested = temp_dir.path().join("dir1").join("dir2");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("deep.txt"), b"content").unwrap();

        // Should find the deeply nested file
        let modified = recovery.recover().unwrap();
        assert_eq!(modified.len(), 1);
        assert!(modified[0]
            .to_str()
            .unwrap()
            .contains("dir1") && modified[0].to_str().unwrap().contains("deep.txt"));
    }

    #[test]
    fn test_should_ignore_paths() {
        let temp_dir = TempDir::new().unwrap();
        let recovery = OverflowRecovery::new(temp_dir.path());

        assert!(recovery.should_ignore(Path::new("/foo/.tl/store.db")));
        assert!(recovery.should_ignore(Path::new("/foo/.git/config")));
        assert!(recovery.should_ignore(Path::new("/foo/target/debug/app")));
        assert!(recovery.should_ignore(Path::new("/foo/node_modules/package")));

        assert!(!recovery.should_ignore(Path::new("/foo/src/main.rs")));
        assert!(!recovery.should_ignore(Path::new("/foo/README.md")));
    }

    #[test]
    fn test_estimate_scan_size() {
        let temp_dir = TempDir::new().unwrap();
        let mut recovery = OverflowRecovery::new(temp_dir.path());

        recovery.mark_checkpoint(SystemTime::now());
        sleep(Duration::from_millis(10));

        // Create 10 files
        for i in 0..10 {
            fs::write(temp_dir.path().join(format!("file{}.txt", i)), b"content").unwrap();
        }

        let estimate = recovery.estimate_scan_size().unwrap();
        assert_eq!(estimate, 10);
    }

    #[test]
    fn test_overflow_recovery_ignores_build_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let mut recovery = OverflowRecovery::new(temp_dir.path());

        recovery.mark_checkpoint(SystemTime::now());
        sleep(Duration::from_millis(10));

        // Create files in ignored directories
        let target_dir = temp_dir.path().join("target");
        fs::create_dir(&target_dir).unwrap();
        fs::write(target_dir.join("ignored.txt"), b"content").unwrap();

        // Create regular file
        fs::write(temp_dir.path().join("included.txt"), b"content").unwrap();

        // Should only find the non-ignored file
        let modified = recovery.recover().unwrap();
        assert_eq!(modified.len(), 1);
        assert!(modified[0].to_str().unwrap().contains("included.txt"));
    }
}
