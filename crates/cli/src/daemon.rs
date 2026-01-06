//! Daemon lifecycle management

use crate::ipc::{handle_connection, DaemonStatus, IpcRequest, IpcResponse, IpcServer};
use crate::locks::{DaemonLock, RestoreLock};
use crate::util;
use anyhow::{Context, Result};
use tl_core::store::Store;
use journal::{incremental_update, Checkpoint, CheckpointMeta, CheckpointReason, Journal, PathMap};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use ulid::Ulid;
use watcher::Watcher;

/// Flush checkpoint request with response channel
type FlushRequest = oneshot::Sender<Result<Option<String>>>;

/// Supervisor for daemon process - handles crashes and restarts
pub struct DaemonSupervisor {
    repo_root: PathBuf,
    max_restarts: usize,
    restart_window: Duration,
}

impl DaemonSupervisor {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            max_restarts: 5,                           // Max 5 restarts
            restart_window: Duration::from_secs(60),   // Within 60s window
        }
    }

    /// Run daemon under supervision with auto-restart
    pub async fn run_supervised(self) -> Result<()> {
        let mut restart_times: Vec<Instant> = Vec::new();

        loop {
            // Clean old restart times outside window
            let now = Instant::now();
            restart_times.retain(|t| now.duration_since(*t) < self.restart_window);

            // Check restart limit
            if restart_times.len() >= self.max_restarts {
                tracing::error!(
                    "Daemon crashed {} times in {}s - giving up",
                    self.max_restarts,
                    self.restart_window.as_secs()
                );
                anyhow::bail!("Too many daemon crashes");
            }

            // Start daemon
            tracing::info!("Starting daemon (supervised)");
            let result = start_daemon_direct(&self.repo_root).await;

            match result {
                Ok(()) => {
                    // Clean shutdown - exit supervisor
                    tracing::info!("Daemon shutdown cleanly");
                    break;
                }
                Err(e) => {
                    // Crash - record and restart
                    tracing::error!("Daemon crashed: {}", e);
                    restart_times.push(Instant::now());

                    // Brief delay before restart
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    tracing::info!("Restarting daemon...");
                }
            }
        }

        Ok(())
    }
}

/// Daemon structure
pub struct Daemon {
    store: Arc<Store>,
    journal: Arc<Journal>,
    watcher: Watcher,
    pathmap: PathMap,

    _lock: DaemonLock,
    ipc_server: IpcServer,

    shutdown_tx: broadcast::Sender<()>,
    shutdown_rx: broadcast::Receiver<()>,

    flush_tx: mpsc::Sender<FlushRequest>,
    flush_rx: mpsc::Receiver<FlushRequest>,

    status: Arc<RwLock<DaemonStatus>>,

    // Performance caching
    checkpoint_count_cache: Arc<AtomicUsize>,
}

impl Daemon {
    /// Run main daemon event loop
    async fn run(mut self) -> Result<()> {
        tracing::info!("Daemon started (PID: {})", std::process::id());

        // Setup signal handlers
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        // Debouncing state - load any pending paths from previous crash
        let tl_dir = self.store.tl_dir().to_path_buf();
        let mut pending_paths: HashSet<Arc<Path>> = load_pending_paths(&tl_dir);
        let mut last_checkpoint = Instant::now();
        let checkpoint_interval = Duration::from_secs(5); // Create checkpoint every 5s
        let mut last_pending_save = Instant::now();
        let pending_save_interval = Duration::from_secs(2); // Save pending paths every 2s

        loop {
            tokio::select! {
                // Watcher events
                _ = self.watcher.poll_events() => {
                    // Collect ready paths
                    let batch = self.watcher.next_batch();
                    if !batch.is_empty() {
                        pending_paths.extend(batch);

                        // Update status
                        self.status.write().await.watcher_paths = pending_paths.len();

                        // Periodically save pending paths for crash recovery
                        if last_pending_save.elapsed() >= pending_save_interval && !pending_paths.is_empty() {
                            if let Err(e) = save_pending_paths(&tl_dir, &pending_paths) {
                                tracing::warn!("Failed to save pending paths: {}", e);
                            }
                            last_pending_save = Instant::now();
                        }
                    }
                }

                // Periodic checkpoint creation
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(last_checkpoint + checkpoint_interval)), if !pending_paths.is_empty() => {
                    // CRITICAL: Check if a restore operation is in progress
                    // If so, skip this checkpoint cycle to prevent race conditions
                    if RestoreLock::is_held(&self.store.tl_dir()) {
                        tracing::info!("Skipping checkpoint - restore operation in progress");
                        continue;
                    }

                    // Check if pathmap needs rebuild (after restore operation)
                    let stale_marker = tl_dir.join("state/pathmap_stale");
                    if stale_marker.exists() {
                        tracing::info!("Pathmap marked stale - rebuilding from HEAD");
                        match rebuild_pathmap_from_head(&tl_dir, &self.journal, &self.store) {
                            Ok(new_pathmap) => {
                                self.pathmap = new_pathmap;
                                let _ = std::fs::remove_file(&stale_marker);
                                tracing::info!("Pathmap rebuilt successfully");
                            }
                            Err(e) => {
                                tracing::error!("Failed to rebuild pathmap: {}", e);
                            }
                        }
                    }

                    tracing::info!("Creating checkpoint for {} paths", pending_paths.len());

                    match self.create_checkpoint(&pending_paths).await {
                        Ok(checkpoint_id) => {
                            tracing::info!("Created checkpoint: {}", checkpoint_id);

                            // Update status
                            let mut status = self.status.write().await;
                            status.checkpoints_created += 1;
                            status.last_checkpoint_time = Some(current_timestamp_ms());

                            pending_paths.clear();
                            last_checkpoint = Instant::now();

                            // Clear saved pending paths after successful checkpoint
                            clear_pending_paths(&tl_dir);
                        }
                        Err(e) => {
                            tracing::error!("Checkpoint creation failed: {}", e);
                            // Save pending paths in case of crash during error recovery
                            let _ = save_pending_paths(&tl_dir, &pending_paths);
                        }
                    }
                }

                // Handle flush checkpoint requests from IPC
                Some(response_tx) = self.flush_rx.recv() => {
                    tracing::info!("Received flush checkpoint request");

                    // Check for restore lock before flushing
                    if RestoreLock::is_held(&self.store.tl_dir()) {
                        tracing::warn!("Flush skipped - restore operation in progress");
                        let _ = response_tx.send(Ok(None));
                        continue;
                    }

                    let result = if !pending_paths.is_empty() {
                        match self.create_checkpoint(&pending_paths).await {
                            Ok(checkpoint_id) => {
                                tracing::info!("Flushed checkpoint: {}", checkpoint_id);

                                // Update status
                                let mut status = self.status.write().await;
                                status.checkpoints_created += 1;
                                status.last_checkpoint_time = Some(current_timestamp_ms());

                                pending_paths.clear();
                                last_checkpoint = Instant::now();

                                // Clear saved pending paths after successful checkpoint
                                clear_pending_paths(&tl_dir);

                                Ok(Some(checkpoint_id.to_string()))
                            }
                            Err(e) => {
                                tracing::error!("Flush checkpoint failed: {}", e);
                                // Save pending paths in case of crash
                                let _ = save_pending_paths(&tl_dir, &pending_paths);
                                Err(e)
                            }
                        }
                    } else {
                        // Nothing to checkpoint
                        Ok(None)
                    };

                    // Send result back to IPC handler (ignore if receiver dropped)
                    let _ = response_tx.send(result);
                }

                // Handle IPC connections
                result = self.ipc_server.accept() => {
                    match result {
                        Ok(stream) => {
                            let journal = Arc::clone(&self.journal);
                            let store = Arc::clone(&self.store);
                            let status = Arc::clone(&self.status);
                            let shutdown_tx = self.shutdown_tx.clone();
                            let flush_tx = self.flush_tx.clone();
                            let checkpoint_count_cache = Arc::clone(&self.checkpoint_count_cache);

                            tokio::spawn(async move {
                                let handler = |request: IpcRequest| async move {
                                    match request {
                                IpcRequest::GetStatus => {
                                    let status = status.read().await.clone();
                                    Ok(IpcResponse::Status(status))
                                }
                                IpcRequest::GetHead => {
                                    match journal.latest() {
                                        Ok(checkpoint) => Ok(IpcResponse::Head(checkpoint)),
                                        Err(e) => Ok(IpcResponse::Error(e.to_string())),
                                    }
                                }
                                IpcRequest::GetCheckpoint(id_str) => {
                                    match Ulid::from_string(&id_str) {
                                        Ok(id) => {
                                            match journal.get(&id) {
                                                Ok(checkpoint) => Ok(IpcResponse::Checkpoint(checkpoint)),
                                                Err(e) => Ok(IpcResponse::Error(e.to_string())),
                                            }
                                        }
                                        Err(e) => Ok(IpcResponse::Error(e.to_string())),
                                    }
                                }
                                IpcRequest::FlushCheckpoint => {
                                    // Request flush from main event loop
                                    let (response_tx, response_rx) = oneshot::channel();

                                    if let Err(_) = flush_tx.send(response_tx).await {
                                        return Ok(IpcResponse::Error("Daemon shutting down".to_string()));
                                    }

                                    // Wait for flush to complete
                                    match response_rx.await {
                                        Ok(Ok(checkpoint_id)) => Ok(IpcResponse::CheckpointFlushed(checkpoint_id)),
                                        Ok(Err(e)) => Ok(IpcResponse::Error(e.to_string())),
                                        Err(_) => Ok(IpcResponse::Error("Flush request cancelled".to_string())),
                                    }
                                }
                                IpcRequest::GetCheckpoints { limit, offset } => {
                                    let mut checkpoints = Vec::new();
                                    let limit = limit.unwrap_or(20);
                                    let offset = offset.unwrap_or(0);

                                    // Walk journal from HEAD backwards
                                    let mut current_id = match journal.latest() {
                                        Ok(Some(cp)) => Some(cp.id),
                                        Ok(None) => None,
                                        Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                    };

                                    let mut count = 0;
                                    while let Some(id) = current_id {
                                        if count >= offset + limit {
                                            break;
                                        }

                                        match journal.get(&id) {
                                            Ok(Some(checkpoint)) => {
                                                if count >= offset {
                                                    checkpoints.push(checkpoint.clone());
                                                }
                                                current_id = checkpoint.parent;
                                                count += 1;
                                            }
                                            Ok(None) => break,
                                            Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                        }
                                    }

                                    Ok(IpcResponse::Checkpoints(checkpoints))
                                }
                                IpcRequest::GetCheckpointCount => {
                                    // Use cached count for O(1) performance
                                    let count = checkpoint_count_cache.load(Ordering::Relaxed);
                                    Ok(IpcResponse::CheckpointCount(count))
                                }
                                IpcRequest::GetCheckpointBatch(ids) => {
                                    // Batch fetch multiple checkpoints in one IPC call
                                    let mut results = Vec::with_capacity(ids.len());

                                    for id_str in ids {
                                        match Ulid::from_string(&id_str) {
                                            Ok(id) => {
                                                let checkpoint = journal.get(&id).ok().flatten();
                                                results.push(checkpoint);
                                            }
                                            Err(_) => results.push(None),
                                        }
                                    }

                                    Ok(IpcResponse::CheckpointBatch(results))
                                }
                                IpcRequest::GetStatusFull => {
                                    // Get all data in one atomic operation
                                    let status = status.read().await.clone();
                                    let head = match journal.latest() {
                                        Ok(checkpoint) => checkpoint,
                                        Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                    };
                                    let count = checkpoint_count_cache.load(Ordering::Relaxed);

                                    Ok(IpcResponse::StatusFull {
                                        status,
                                        head,
                                        checkpoint_count: count,
                                    })
                                }
                                IpcRequest::GetLogData { limit, offset } => {
                                    // Get count and checkpoints in one call
                                    let count = checkpoint_count_cache.load(Ordering::Relaxed);

                                    // Get checkpoints (same logic as GetCheckpoints)
                                    let mut checkpoints = Vec::new();
                                    let limit = limit.unwrap_or(20);
                                    let offset = offset.unwrap_or(0);

                                    // Walk journal from HEAD backwards
                                    let mut current_id = match journal.latest() {
                                        Ok(Some(cp)) => Some(cp.id),
                                        Ok(None) => None,
                                        Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                    };

                                    let mut idx = 0;
                                    while let Some(id) = current_id {
                                        if idx >= offset + limit {
                                            break;
                                        }

                                        match journal.get(&id) {
                                            Ok(Some(checkpoint)) => {
                                                if idx >= offset {
                                                    checkpoints.push(checkpoint.clone());
                                                }
                                                current_id = checkpoint.parent;
                                                idx += 1;
                                            }
                                            Ok(None) => break,
                                            Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                        }
                                    }

                                    Ok(IpcResponse::LogData { count, checkpoints })
                                }
                                IpcRequest::ResolveCheckpointRefs(refs) => {
                                    // Resolve each reference (full ULID, short prefix, or pin name)
                                    let tl_dir = store.tl_dir();
                                    let pin_manager = journal::PinManager::new(&tl_dir);
                                    let mut results = Vec::new();

                                    for checkpoint_ref in refs {
                                        // Handle HEAD alias
                                        if checkpoint_ref == "HEAD" {
                                            match journal.latest() {
                                                Ok(Some(cp)) => {
                                                    results.push(Some(cp));
                                                    continue;
                                                }
                                                Ok(None) | Err(_) => {
                                                    results.push(None);
                                                    continue;
                                                }
                                            }
                                        }

                                        // Handle HEAD~N syntax (N checkpoints before HEAD)
                                        if checkpoint_ref.starts_with("HEAD~") {
                                            if let Some(n_str) = checkpoint_ref.strip_prefix("HEAD~") {
                                                if let Ok(n) = n_str.parse::<usize>() {
                                                    // Walk back N checkpoints from HEAD
                                                    let mut current = journal.latest().ok().flatten();
                                                    for _ in 0..n {
                                                        if let Some(cp) = &current {
                                                            if let Some(parent_id) = cp.parent {
                                                                current = journal.get(&parent_id).ok().flatten();
                                                            } else {
                                                                current = None;
                                                                break;
                                                            }
                                                        } else {
                                                            break;
                                                        }
                                                    }
                                                    results.push(current);
                                                    continue;
                                                }
                                            }
                                            results.push(None); // Invalid HEAD~N format
                                            continue;
                                        }

                                        // Try full ULID first
                                        if let Ok(ulid) = Ulid::from_string(&checkpoint_ref) {
                                            match journal.get(&ulid) {
                                                Ok(Some(cp)) => {
                                                    results.push(Some(cp));
                                                    continue;
                                                }
                                                _ => {}
                                            }
                                        }

                                        // Try short prefix (4+ chars)
                                        if checkpoint_ref.len() >= 4 {
                                            let all_ids = match journal.all_checkpoint_ids() {
                                                Ok(ids) => ids,
                                                Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                            };

                                            let matching: Vec<_> = all_ids
                                                .iter()
                                                .filter(|id| id.to_string().starts_with(&checkpoint_ref))
                                                .collect();

                                            if matching.len() == 1 {
                                                match journal.get(matching[0]) {
                                                    Ok(Some(cp)) => {
                                                        results.push(Some(cp));
                                                        continue;
                                                    }
                                                    _ => {}
                                                }
                                            } else if matching.len() > 1 {
                                                results.push(None); // Ambiguous
                                                continue;
                                            }
                                        }

                                        // Try pin name
                                        match pin_manager.list_pins() {
                                            Ok(pins) => {
                                                if let Some((_, ulid)) = pins.iter().find(|(name, _)| name == &checkpoint_ref) {
                                                    match journal.get(ulid) {
                                                        Ok(Some(cp)) => {
                                                            results.push(Some(cp));
                                                            continue;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }

                                        // Not found
                                        results.push(None);
                                    }

                                    Ok(IpcResponse::ResolvedCheckpoints(results))
                                }
                                IpcRequest::GetInfoData => {
                                    // Get checkpoint count from cache
                                    let total_checkpoints = checkpoint_count_cache.load(Ordering::Relaxed);

                                    // Get all checkpoint IDs
                                    let checkpoint_ids = match journal.all_checkpoint_ids() {
                                        Ok(ids) => ids.iter().map(|id| id.to_string()).collect(),
                                        Err(e) => return Ok(IpcResponse::Error(e.to_string())),
                                    };

                                    // Calculate store size
                                    let store_path = store.tl_dir().join("store");
                                    let store_size_bytes = calculate_dir_size(&store_path).unwrap_or(0);

                                    Ok(IpcResponse::InfoData {
                                        total_checkpoints,
                                        checkpoint_ids,
                                        store_size_bytes,
                                    })
                                }
                                IpcRequest::Shutdown => {
                                    // Signal shutdown
                                    let _ = shutdown_tx.send(());
                                    Ok(IpcResponse::Ok)
                                }
                                IpcRequest::InvalidatePathmap => {
                                    // Mark pathmap as needing rebuild
                                    // This is called after restore operations modify the working directory
                                    tracing::info!("Pathmap invalidation requested - will rebuild from HEAD");

                                    // Write a marker file that the daemon checks on next checkpoint
                                    let marker_path = store.tl_dir().join("state/pathmap_stale");
                                    if let Err(e) = std::fs::write(&marker_path, "stale") {
                                        return Ok(IpcResponse::Error(format!("Failed to mark pathmap stale: {}", e)));
                                    }

                                    Ok(IpcResponse::Ok)
                                }
                            }
                        };

                            if let Err(e) = handle_connection(stream, handler).await {
                                tracing::error!("IPC connection error: {}", e);
                            }
                        });
                        }
                        Err(e) => {
                            tracing::error!("IPC accept error: {}", e);
                            // Small delay to prevent tight loop if accept keeps failing
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }

                // Shutdown signals
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM, shutting down");
                    break;
                }
                _ = sigint.recv() => {
                    tracing::info!("Received SIGINT, shutting down");
                    break;
                }
                _ = self.shutdown_rx.recv() => {
                    tracing::info!("Received shutdown signal via IPC");
                    break;
                }
            }
        }

        // Graceful shutdown
        self.shutdown().await?;

        Ok(())
    }

    /// Create a checkpoint from dirty paths
    async fn create_checkpoint(&mut self, dirty_paths: &HashSet<Arc<Path>>) -> Result<Ulid> {
        // Convert Arc<Path> to &Path
        let paths: Vec<&Path> = dirty_paths.iter().map(|p| p.as_ref()).collect();

        // Use incremental update algorithm
        let (new_map, _tree, tree_hash) = incremental_update(
            &self.pathmap,
            paths,
            self.store.root(),
            &self.store,
        )?;

        // Get parent checkpoint
        let parent_id = self.journal.latest()?.map(|cp| cp.id);

        // Create checkpoint metadata
        let meta = CheckpointMeta {
            files_changed: dirty_paths.len() as u32,
            bytes_added: 0,    // TODO: Track from incremental_update
            bytes_removed: 0,  // TODO: Track from incremental_update
        };

        // Create checkpoint
        let checkpoint = Checkpoint::new(
            parent_id,
            tree_hash,
            CheckpointReason::FsBatch,
            dirty_paths.iter().map(|p| p.to_path_buf()).collect(),
            meta,
        );

        // Append to journal
        self.journal.append(&checkpoint)?;

        // Update checkpoint count cache atomically
        self.checkpoint_count_cache.fetch_add(1, Ordering::Relaxed);

        // Update pathmap (atomic swap)
        self.pathmap = new_map;

        // Save pathmap to disk
        save_pathmap(&self.store.tl_dir().join("state/pathmap.bin"), &self.pathmap)?;

        // Mark checkpoint time for watcher overflow recovery
        self.watcher.mark_checkpoint(SystemTime::now());

        Ok(checkpoint.id)
    }

    /// Graceful shutdown
    async fn shutdown(mut self) -> Result<()> {
        tracing::info!("Starting graceful shutdown");

        // 1. Flush any pending paths
        let pending = self.watcher.flush();
        if !pending.is_empty() {
            tracing::info!("Flushing {} pending paths", pending.len());
            let pending_set: HashSet<_> = pending.into_iter().collect();
            if let Err(e) = self.create_checkpoint(&pending_set).await {
                tracing::error!("Failed to create final checkpoint: {}", e);
            }
        }

        // 2. Stop watcher
        self.watcher.stop().await?;

        // 3. Lock will be released when dropped

        tracing::info!("Shutdown complete");
        Ok(())
    }
}

/// Start daemon in background, returns immediately after spawning
/// Logs are redirected to .tl/logs/daemon.log
pub(crate) async fn start_background_internal(repo_root: &Path) -> Result<()> {
    use std::process::Command;

    let log_file = repo_root.join(".tl/logs/daemon.log");

    // Ensure logs directory exists
    std::fs::create_dir_all(repo_root.join(".tl/logs"))
        .context("Failed to create logs directory")?;

    // Get current executable path
    let exe = std::env::current_exe()
        .context("Failed to get current executable path")?;

    // Spawn daemon in background with nohup
    let log_file_writer = std::fs::File::create(&log_file)
        .context("Failed to create log file")?;

    let mut cmd = Command::new("nohup");
    cmd.arg(&exe)
        .arg("start")
        .arg("--foreground")
        .stdout(log_file_writer.try_clone()?)
        .stderr(log_file_writer);

    // Pass SSH agent environment variables so daemon can authenticate
    // These are needed for git operations (push/pull) via JJ
    if let Ok(ssh_auth_sock) = std::env::var("SSH_AUTH_SOCK") {
        cmd.env("SSH_AUTH_SOCK", ssh_auth_sock);
    }
    if let Ok(ssh_agent_pid) = std::env::var("SSH_AGENT_PID") {
        cmd.env("SSH_AGENT_PID", ssh_agent_pid);
    }

    cmd.spawn()
        .context("Failed to spawn daemon process")?;

    Ok(())
}

/// Ensure daemon is running, auto-start if needed (default 1s timeout)
pub async fn ensure_daemon_running() -> Result<()> {
    ensure_daemon_running_with_timeout(1).await
}

/// Ensure daemon is running with configurable timeout
pub async fn ensure_daemon_running_with_timeout(timeout_secs: u64) -> Result<()> {
    // Check if daemon is already running
    if is_running().await {
        return Ok(());
    }

    // Find repo root
    let repo_root = util::find_repo_root()?;

    // Start daemon in background
    start_background_internal(&repo_root).await?;

    // Wait with timeout for startup confirmation
    let timeout = Duration::from_secs(timeout_secs);
    let start = Instant::now();

    while start.elapsed() < timeout {
        if is_running().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Timeout - daemon didn't start
    let log_file = repo_root.join(".tl/logs/daemon.log");
    anyhow::bail!(
        "Daemon failed to start within {}s (check logs at {})",
        timeout_secs,
        log_file.display()
    )
}

/// Start the Timelapse daemon (public entry point)
pub async fn start(supervised: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    if supervised {
        // Run under supervisor
        let supervisor = DaemonSupervisor::new(repo_root);
        supervisor.run_supervised().await
    } else {
        // Direct start (existing logic)
        start_daemon_direct(&repo_root).await
    }
}

/// Start daemon directly without supervision (internal)
async fn start_daemon_direct(repo_root: &Path) -> Result<()> {
    let tl_dir = repo_root.join(".tl");

    // 2. Check if already running
    if is_running_impl(&tl_dir).await {
        anyhow::bail!("Daemon already running");
    }

    // 3. Acquire daemon lock
    let lock = DaemonLock::acquire(&tl_dir)
        .context("Failed to acquire daemon lock")?;

    // 4. Initialize components
    let store = Arc::new(
        Store::open(&repo_root).context("Failed to open store")?,
    );
    let journal = Arc::new(
        Journal::open(&tl_dir.join("journal")).context("Failed to open journal")?,
    );

    // Load or create initial pathmap
    let pathmap = load_or_create_pathmap(&tl_dir, &journal)?;

    // 5. Initialize watcher
    let mut watcher = Watcher::new(&repo_root)
        .context("Failed to create watcher")?;
    watcher.start().await.context("Failed to start watcher")?;

    // 6. Create daemon status and channels
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let (flush_tx, flush_rx) = mpsc::channel(10);  // Buffer up to 10 flush requests

    let start_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let status = Arc::new(RwLock::new(DaemonStatus {
        running: true,
        pid: std::process::id(),
        start_time_ms,
        checkpoints_created: 0,
        last_checkpoint_time: None,
        watcher_paths: 0,
    }));

    // Initialize checkpoint count cache
    let initial_count = journal.count();
    let checkpoint_count_cache = Arc::new(AtomicUsize::new(initial_count));

    // 7. Start IPC server
    let socket_path = tl_dir.join("state/daemon.sock");
    let ipc_server = IpcServer::start(&socket_path)
        .await
        .context("Failed to start IPC server")?;

    // 8. Create and run daemon
    let daemon = Daemon {
        store,
        journal,
        watcher,
        pathmap,
        _lock: lock,
        ipc_server,
        shutdown_tx,
        shutdown_rx,
        flush_tx,
        flush_rx,
        status,
        checkpoint_count_cache,
    };

    daemon.run().await?;

    Ok(())
}

/// Stop the Timelapse daemon
pub async fn stop() -> Result<()> {
    let repo_root = util::find_repo_root()?;
    let socket_path = repo_root.join(".tl/state/daemon.sock");

    // Check if daemon is running
    if !socket_path.exists() {
        println!("Daemon is not running");
        return Ok(());
    }

    // Connect and send shutdown
    let mut client = crate::ipc::IpcClient::connect(&socket_path).await?;
    client.shutdown().await?;

    // Wait for daemon to exit (poll lock file)
    let lock_path = repo_root.join(".tl/locks/daemon.lock");
    let timeout = Duration::from_secs(5);
    let start = Instant::now();

    while lock_path.exists() && start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if lock_path.exists() {
        tracing::warn!("Daemon did not exit gracefully");
        anyhow::bail!("Daemon shutdown timeout");
    }

    println!("Daemon stopped successfully");
    Ok(())
}

/// Check if daemon is running
pub async fn is_running() -> bool {
    match util::find_repo_root() {
        Ok(repo_root) => is_running_impl(&repo_root.join(".tl")).await,
        Err(_) => false,
    }
}

async fn is_running_impl(tl_dir: &Path) -> bool {
    // Check both lock file and socket
    let lock_path = tl_dir.join("locks/daemon.lock");
    let socket_path = tl_dir.join("state/daemon.sock");

    if !lock_path.exists() || !socket_path.exists() {
        return false;
    }

    // Verify we can connect to IPC
    match crate::ipc::IpcClient::connect(&socket_path).await {
        Ok(_) => true,
        Err(_) => false,
    }
}

/// Load pathmap from disk or create new one
fn load_or_create_pathmap(tl_dir: &Path, journal: &Journal) -> Result<PathMap> {
    let pathmap_path = tl_dir.join("state/pathmap.bin");

    if pathmap_path.exists() {
        // Load existing pathmap
        match PathMap::load(&pathmap_path) {
            Ok(map) => {
                tracing::info!("Loaded pathmap with {} entries", map.len());
                Ok(map)
            }
            Err(e) => {
                tracing::warn!("Failed to load pathmap ({}), creating new", e);
                // Get root tree hash from latest checkpoint, or use zero hash
                let root_hash = journal
                    .latest()?
                    .map(|cp| cp.root_tree)
                    .unwrap_or_else(|| tl_core::hash::Sha1Hash::from_bytes([0u8; 20]));
                Ok(PathMap::new(root_hash))
            }
        }
    } else {
        // Create new pathmap
        tracing::info!("Creating new pathmap");
        // Get root tree hash from latest checkpoint, or use zero hash
        let root_hash = journal
            .latest()?
            .map(|cp| cp.root_tree)
            .unwrap_or_else(|| tl_core::hash::Sha1Hash::from_bytes([0u8; 20]));
        Ok(PathMap::new(root_hash))
    }
}

/// Save pathmap to disk
fn save_pathmap(path: &Path, pathmap: &PathMap) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    pathmap
        .save(path)
        .context("Failed to save pathmap")?;

    Ok(())
}

/// Calculate total size of a directory recursively
fn calculate_dir_size(path: &Path) -> Result<u64> {
    let mut total_size = 0u64;

    if !path.exists() {
        return Ok(0);
    }

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total_size += metadata.len();
        } else if metadata.is_dir() {
            total_size += calculate_dir_size(&entry.path())?;
        }
    }
    Ok(total_size)
}

/// Get current timestamp in milliseconds
fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System time before UNIX epoch")
        .as_millis() as u64
}

// =============================================================================
// Pending Paths Persistence (Fix 11)
// =============================================================================

/// Path to pending paths file
fn pending_paths_file(tl_dir: &Path) -> PathBuf {
    tl_dir.join("state/pending_paths.json")
}

/// Save pending paths to disk for crash recovery
///
/// CRITICAL: This ensures that if the daemon crashes during checkpoint creation,
/// the pending paths are not lost and can be recovered on restart.
fn save_pending_paths(tl_dir: &Path, paths: &HashSet<Arc<Path>>) -> Result<()> {
    let file_path = pending_paths_file(tl_dir);

    // Ensure parent directory exists
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Convert to Vec<String> for JSON serialization
    let paths_vec: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let json = serde_json::to_string(&paths_vec)?;
    std::fs::write(&file_path, json)?;

    tracing::debug!("Saved {} pending paths to disk", paths.len());
    Ok(())
}

/// Load pending paths from disk (for crash recovery)
fn load_pending_paths(tl_dir: &Path) -> HashSet<Arc<Path>> {
    let file_path = pending_paths_file(tl_dir);

    if !file_path.exists() {
        return HashSet::new();
    }

    match std::fs::read_to_string(&file_path) {
        Ok(json) => {
            match serde_json::from_str::<Vec<String>>(&json) {
                Ok(paths_vec) => {
                    let paths: HashSet<Arc<Path>> = paths_vec
                        .into_iter()
                        .map(|s| Arc::from(PathBuf::from(s).as_path()))
                        .collect();

                    if !paths.is_empty() {
                        tracing::info!("Recovered {} pending paths from previous crash", paths.len());
                    }
                    paths
                }
                Err(e) => {
                    tracing::warn!("Failed to parse pending paths: {}", e);
                    HashSet::new()
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read pending paths file: {}", e);
            HashSet::new()
        }
    }
}

/// Clear pending paths file after successful checkpoint
fn clear_pending_paths(tl_dir: &Path) {
    let file_path = pending_paths_file(tl_dir);
    if file_path.exists() {
        let _ = std::fs::remove_file(&file_path);
    }
}

// =============================================================================
// Pathmap Rebuild (Fix 12)
// =============================================================================

/// Rebuild pathmap from HEAD checkpoint after restore operation
///
/// CRITICAL: After a restore operation modifies the working directory,
/// the in-memory pathmap becomes stale. This function rebuilds it from
/// the HEAD checkpoint's tree to ensure subsequent checkpoints correctly
/// capture only actual changes.
fn rebuild_pathmap_from_head(
    tl_dir: &Path,
    journal: &Journal,
    store: &Store,
) -> Result<PathMap> {
    // Get latest checkpoint
    let head = journal.latest()?.ok_or_else(|| {
        anyhow::anyhow!("No checkpoint exists - cannot rebuild pathmap")
    })?;

    // Load the tree for HEAD checkpoint
    let tree = store.read_tree(head.root_tree)?;

    // Create new PathMap from tree
    let new_pathmap = PathMap::from_tree(&tree, head.root_tree);

    // Save the rebuilt pathmap to disk
    let pathmap_path = tl_dir.join("state/pathmap.bin");
    new_pathmap.save(&pathmap_path)?;

    tracing::info!(
        "Rebuilt pathmap from HEAD checkpoint {} ({} entries)",
        head.id,
        new_pathmap.len()
    );

    Ok(new_pathmap)
}
