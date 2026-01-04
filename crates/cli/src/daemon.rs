//! Daemon lifecycle management

use crate::ipc::{handle_connection, DaemonStatus, IpcRequest, IpcResponse, IpcServer};
use crate::locks::DaemonLock;
use crate::util;
use anyhow::{Context, Result};
use tl_core::store::Store;
use journal::{incremental_update, Checkpoint, CheckpointMeta, CheckpointReason, Journal, PathMap};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, RwLock};
use ulid::Ulid;
use watcher::Watcher;

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

    status: Arc<RwLock<DaemonStatus>>,
}

impl Daemon {
    /// Run main daemon event loop
    async fn run(mut self) -> Result<()> {
        tracing::info!("Daemon started (PID: {})", std::process::id());

        // Setup signal handlers
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        // Debouncing state
        let mut pending_paths: HashSet<Arc<Path>> = HashSet::new();
        let mut last_checkpoint = Instant::now();
        let checkpoint_interval = Duration::from_secs(5); // Create checkpoint every 5s

        let start_time = Instant::now();

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
                    }
                }

                // Periodic checkpoint creation
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(last_checkpoint + checkpoint_interval)), if !pending_paths.is_empty() => {
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
                        }
                        Err(e) => {
                            tracing::error!("Checkpoint creation failed: {}", e);
                        }
                    }
                }

                // Update uptime periodically
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    self.status.write().await.uptime_secs = start_time.elapsed().as_secs();
                }

                // Handle IPC connections
                Ok(stream) = self.ipc_server.accept() => {
                    let journal = Arc::clone(&self.journal);
                    let status = Arc::clone(&self.status);
                    let shutdown_tx = self.shutdown_tx.clone();

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
                                IpcRequest::Shutdown => {
                                    // Signal shutdown
                                    let _ = shutdown_tx.send(());
                                    Ok(IpcResponse::Ok)
                                }
                            }
                        };

                        if let Err(e) = handle_connection(stream, handler).await {
                            tracing::error!("IPC connection error: {}", e);
                        }
                    });
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

/// Start the Timelapse daemon
pub async fn start() -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;
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

    // 6. Create daemon status
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

    let status = Arc::new(RwLock::new(DaemonStatus {
        running: true,
        pid: std::process::id(),
        uptime_secs: 0,
        checkpoints_created: 0,
        last_checkpoint_time: None,
        watcher_paths: 0,
    }));

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
        status,
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

/// Get current timestamp in milliseconds
fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System time before UNIX epoch")
        .as_millis() as u64
}
