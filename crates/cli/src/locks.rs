//! Lock file management for daemon exclusivity

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Daemon lock file structure
pub struct DaemonLock {
    path: PathBuf,
    #[allow(dead_code)]
    file: File,
}

/// Lock file content
#[derive(Serialize, Deserialize)]
struct LockContent {
    pid: u32,
    started_at: u64,
}

impl DaemonLock {
    /// Acquire exclusive daemon lock
    ///
    /// Returns error if:
    /// - Lock is already held by a running process
    /// - Permission denied
    pub fn acquire(tl_dir: &Path) -> Result<Self> {
        let lock_path = tl_dir.join("locks/daemon.lock");

        // Ensure locks directory exists
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create locks directory")?;
        }

        // Try to open/create lock file
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .context("Failed to open lock file")?;

        // Try to acquire exclusive lock (non-blocking)
        if !try_flock_exclusive(&file)? {
            // Lock held - check if stale
            if Self::is_stale_lock(&mut file)? {
                // Force remove stale lock and retry
                tracing::warn!("Removing stale daemon lock");
                drop(file);
                std::fs::remove_file(&lock_path)?;
                return Self::acquire(tl_dir); // Retry
            } else {
                anyhow::bail!("Daemon already running (lock file held by active process)");
            }
        }

        // Write PID to lock file
        Self::write_lock_content(&mut file)?;

        Ok(Self {
            path: lock_path,
            file,
        })
    }

    /// Release the daemon lock
    pub fn release(self) -> Result<()> {
        // File lock is automatically released when file is dropped
        // But explicitly remove the file
        std::fs::remove_file(&self.path)
            .context("Failed to remove lock file")?;
        Ok(())
    }

    /// Check if lock file represents a stale lock
    fn is_stale_lock(file: &mut File) -> Result<bool> {
        // Read lock content
        match Self::read_lock_content(file) {
            Ok(content) => {
                // Check if process is alive
                Ok(!is_process_alive(content.pid))
            }
            Err(_) => {
                // If we can't read lock content, assume it's stale
                Ok(true)
            }
        }
    }

    /// Write lock content (PID + timestamp)
    fn write_lock_content(file: &mut File) -> Result<()> {
        let content = LockContent {
            pid: std::process::id(),
            started_at: current_timestamp_ms(),
        };

        let serialized = serde_json::to_string(&content)
            .context("Failed to serialize lock content")?;

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Read lock content from file
    fn read_lock_content(file: &mut File) -> Result<LockContent> {
        file.seek(SeekFrom::Start(0))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let content: LockContent = serde_json::from_str(&contents)
            .context("Failed to deserialize lock content")?;
        Ok(content)
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // Ensure lock file is removed on drop
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Try to acquire exclusive file lock (non-blocking)
#[cfg(unix)]
fn try_flock_exclusive(file: &File) -> Result<bool> {
    use nix::fcntl::{flock, FlockArg};
    use std::os::unix::io::AsRawFd;

    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(_) => Ok(true),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Check if process is alive
#[cfg(target_os = "macos")]
fn is_process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // Send signal 0 (null signal) - checks existence without killing
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false, // No such process
        Err(_) => true,                         // Permission denied or other - assume alive
    }
}

#[cfg(target_os = "linux")]
fn is_process_alive(pid: u32) -> bool {
    // Check /proc/<pid> directory exists
    Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn is_process_alive(_pid: u32) -> bool {
    // Conservative: assume process is alive on unknown platforms
    true
}

/// Get current timestamp in milliseconds
fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time before UNIX epoch")
        .as_millis() as u64
}

/// Restore operation lock - prevents daemon checkpoints during restore
///
/// CRITICAL: This lock prevents race conditions between restore operations
/// and daemon checkpointing. Without this, the daemon could:
/// 1. Create a checkpoint with partially restored files
/// 2. Update pathmap with incorrect state
pub struct RestoreLock {
    path: PathBuf,
    #[allow(dead_code)]
    file: File,
}

impl RestoreLock {
    /// Acquire restore lock (non-blocking, fails if already held)
    pub fn acquire(tl_dir: &Path) -> Result<Self> {
        let lock_path = tl_dir.join("locks/restore.lock");

        // Ensure locks directory exists
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create locks directory")?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .context("Failed to open restore lock file")?;

        // Try to acquire exclusive lock (non-blocking)
        if !try_flock_exclusive(&file)? {
            anyhow::bail!("Another restore operation is in progress");
        }

        // Write timestamp for debugging
        let mut file = file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "{}", current_timestamp_ms())?;
        file.sync_all()?;

        Ok(Self {
            path: lock_path,
            file,
        })
    }

    /// Check if restore lock is currently held (for daemon to check)
    pub fn is_held(tl_dir: &Path) -> bool {
        let lock_path = tl_dir.join("locks/restore.lock");
        if !lock_path.exists() {
            return false;
        }

        // Try to acquire the lock - if we can, it's not held
        match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(file) => {
                match try_flock_exclusive(&file) {
                    Ok(acquired) => {
                        // If we acquired it, release immediately and return false
                        // If we couldn't acquire, it's held
                        !acquired
                    }
                    Err(_) => true, // Assume held on error
                }
            }
            Err(_) => true, // Assume held if we can't open
        }
    }
}

impl Drop for RestoreLock {
    fn drop(&mut self) {
        // Remove lock file on drop
        let _ = std::fs::remove_file(&self.path);
    }
}

/// GC operation lock - ensures exclusive access during garbage collection
///
/// CRITICAL: GC must have exclusive access to prevent:
/// 1. Other operations accessing objects being deleted
/// 2. New objects being created that reference deleted objects
pub struct GcLock {
    path: PathBuf,
    #[allow(dead_code)]
    file: File,
}

impl GcLock {
    /// Acquire GC lock (non-blocking, fails if already held)
    pub fn acquire(tl_dir: &Path) -> Result<Self> {
        let lock_path = tl_dir.join("locks/gc.lock");

        // Ensure locks directory exists
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create locks directory")?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .context("Failed to open GC lock file")?;

        // Try to acquire exclusive lock (non-blocking)
        if !try_flock_exclusive(&file)? {
            anyhow::bail!("Another GC operation is in progress");
        }

        // Write timestamp for debugging
        let mut file = file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "{}", current_timestamp_ms())?;
        file.sync_all()?;

        Ok(Self {
            path: lock_path,
            file,
        })
    }

    /// Check if GC lock is currently held
    pub fn is_held(tl_dir: &Path) -> bool {
        let lock_path = tl_dir.join("locks/gc.lock");
        if !lock_path.exists() {
            return false;
        }

        match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(file) => {
                match try_flock_exclusive(&file) {
                    Ok(acquired) => !acquired,
                    Err(_) => true,
                }
            }
            Err(_) => true,
        }
    }
}

impl Drop for GcLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Init operation lock - prevents concurrent init operations
pub struct InitLock {
    path: PathBuf,
    #[allow(dead_code)]
    file: File,
}

impl InitLock {
    /// Acquire init lock at repo root level
    pub fn acquire(repo_root: &Path) -> Result<Self> {
        let lock_path = repo_root.join(".tl-init.lock");

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .context("Failed to open init lock file")?;

        // Try to acquire exclusive lock (non-blocking)
        if !try_flock_exclusive(&file)? {
            anyhow::bail!("Another init operation is in progress");
        }

        Ok(Self {
            path: lock_path,
            file,
        })
    }
}

impl Drop for InitLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_lock_acquisition() {
        let temp_dir = TempDir::new().unwrap();
        let tl_dir = temp_dir.path();

        // First lock should succeed
        let lock1 = DaemonLock::acquire(tl_dir);
        assert!(lock1.is_ok());

        // Second lock should fail (same process, but lock is held)
        let lock2 = DaemonLock::acquire(tl_dir);
        assert!(lock2.is_err());

        // Release first lock
        drop(lock1);

        // Now second lock should succeed
        let lock3 = DaemonLock::acquire(tl_dir);
        assert!(lock3.is_ok());
    }

    #[test]
    fn test_lock_release() {
        let temp_dir = TempDir::new().unwrap();
        let tl_dir = temp_dir.path();

        let lock = DaemonLock::acquire(tl_dir).unwrap();
        let lock_path = lock.path.clone();

        // Lock file should exist
        assert!(lock_path.exists());

        // Release lock
        lock.release().unwrap();

        // Lock file should be removed
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_lock_content() {
        let temp_dir = TempDir::new().unwrap();
        let lock_file = temp_dir.path().join("test.lock");

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_file)
            .unwrap();

        // Write lock content
        DaemonLock::write_lock_content(&mut file).unwrap();

        // Read it back
        let content = DaemonLock::read_lock_content(&mut file).unwrap();

        assert_eq!(content.pid, std::process::id());
        assert!(content.started_at > 0);
    }

    #[test]
    fn test_process_alive_current() {
        // Current process should be alive
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn test_process_alive_nonexistent() {
        // PID 999999 is unlikely to exist
        assert!(!is_process_alive(999999));
    }
}
