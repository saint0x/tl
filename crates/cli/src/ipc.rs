//! IPC between CLI and daemon using Unix sockets

use anyhow::{Context, Result};
use journal::Checkpoint;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Maximum IPC message size (10MB)
const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

/// IPC request from CLI to daemon
#[derive(Debug, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Get daemon status
    GetStatus,
    /// Get latest checkpoint (HEAD)
    GetHead,
    /// Get specific checkpoint by ID
    GetCheckpoint(String),
    /// Flush pending changes and create checkpoint immediately
    FlushCheckpoint,
    /// Request graceful shutdown
    Shutdown,
    /// Get checkpoints with pagination (for log)
    GetCheckpoints {
        limit: Option<usize>,
        offset: Option<usize>,
    },
    /// Get total checkpoint count (cached)
    GetCheckpointCount,
    /// Get multiple checkpoints in one call (for diff)
    GetCheckpointBatch(Vec<String>),
    /// Get full status info (status + head + count) in one call
    GetStatusFull,
    /// Get checkpoint count and list in one call (for log)
    GetLogData {
        limit: Option<usize>,
        offset: Option<usize>,
    },
    /// Resolve checkpoint references and return checkpoints (supports short IDs, full IDs, pin names)
    ResolveCheckpointRefs(Vec<String>),
    /// Get repository info (checkpoint IDs, storage stats)
    GetInfoData,
    /// Invalidate pathmap (after restore operation modifies working directory)
    /// Daemon will rebuild pathmap from HEAD checkpoint on next checkpoint cycle
    InvalidatePathmap,
}

/// IPC response from daemon to CLI
#[derive(Debug, Serialize, Deserialize)]
pub enum IpcResponse {
    /// Daemon status information
    Status(DaemonStatus),
    /// HEAD checkpoint (if exists)
    Head(Option<Checkpoint>),
    /// Specific checkpoint (if exists)
    Checkpoint(Option<Checkpoint>),
    /// Checkpoint ID created from flush (None if nothing to checkpoint)
    CheckpointFlushed(Option<String>),
    /// Simple acknowledgment
    Ok,
    /// List of checkpoints (for pagination)
    Checkpoints(Vec<Checkpoint>),
    /// Total checkpoint count
    CheckpointCount(usize),
    /// Batch of checkpoints (same order as request, None if not found)
    CheckpointBatch(Vec<Option<Checkpoint>>),
    /// Full status information
    StatusFull {
        status: DaemonStatus,
        head: Option<Checkpoint>,
        checkpoint_count: usize,
    },
    /// Log data (count + checkpoints)
    LogData {
        count: usize,
        checkpoints: Vec<Checkpoint>,
    },
    /// Resolved checkpoints from references (same order as request, None if not found/ambiguous)
    ResolvedCheckpoints(Vec<Option<Checkpoint>>),
    /// Repository info data
    InfoData {
        total_checkpoints: usize,
        checkpoint_ids: Vec<String>,
        store_size_bytes: u64,
    },
    /// Error occurred
    Error(String),
}

/// Daemon status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Is daemon running
    pub running: bool,
    /// Process ID
    pub pid: u32,
    /// Daemon start time (ms since epoch)
    pub start_time_ms: u64,
    /// Total checkpoints created by this daemon instance
    pub checkpoints_created: u64,
    /// Timestamp of last checkpoint (ms since epoch)
    pub last_checkpoint_time: Option<u64>,
    /// Number of paths currently being watched
    pub watcher_paths: usize,
    /// Number of checkpoints skipped due to GC/restore locks
    pub checkpoints_skipped: u64,
}

/// IPC client for CLI to communicate with daemon
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    /// Connect to daemon Unix socket
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .context("Failed to connect to daemon socket")?;

        Ok(Self { stream })
    }

    /// Send request and receive response
    pub async fn send_request(&mut self, request: &IpcRequest) -> Result<IpcResponse> {
        // Serialize request
        let payload = bincode::serialize(request)
            .context("Failed to serialize request")?;

        if payload.len() > MAX_MESSAGE_SIZE {
            anyhow::bail!("Request too large: {} bytes", payload.len());
        }

        // Write length prefix (4 bytes, little-endian)
        let len = (payload.len() as u32).to_le_bytes();
        self.stream
            .write_all(&len)
            .await
            .context("Failed to write request length")?;

        // Write payload
        self.stream
            .write_all(&payload)
            .await
            .context("Failed to write request payload")?;

        self.stream
            .flush()
            .await
            .context("Failed to flush request")?;

        // Read response length
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("Failed to read response length")?;

        let response_len = u32::from_le_bytes(len_buf) as usize;

        if response_len > MAX_MESSAGE_SIZE {
            anyhow::bail!("Response too large: {} bytes", response_len);
        }

        // Read response payload
        let mut response_payload = vec![0u8; response_len];
        self.stream
            .read_exact(&mut response_payload)
            .await
            .context("Failed to read response payload")?;

        // Deserialize response
        let response: IpcResponse = bincode::deserialize(&response_payload)
            .context("Failed to deserialize response")?;

        Ok(response)
    }

    /// Get daemon status
    pub async fn get_status(&mut self) -> Result<DaemonStatus> {
        match self.send_request(&IpcRequest::GetStatus).await? {
            IpcResponse::Status(status) => Ok(status),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetStatus"),
        }
    }

    /// Request daemon shutdown
    pub async fn shutdown(&mut self) -> Result<()> {
        match self.send_request(&IpcRequest::Shutdown).await? {
            IpcResponse::Ok => Ok(()),
            IpcResponse::Error(err) => anyhow::bail!("Shutdown error: {}", err),
            _ => anyhow::bail!("Unexpected response to Shutdown"),
        }
    }

    /// Invalidate pathmap (after restore modifies working directory)
    pub async fn invalidate_pathmap(&mut self) -> Result<()> {
        match self.send_request(&IpcRequest::InvalidatePathmap).await? {
            IpcResponse::Ok => Ok(()),
            IpcResponse::Error(err) => anyhow::bail!("Invalidate pathmap error: {}", err),
            _ => anyhow::bail!("Unexpected response to InvalidatePathmap"),
        }
    }

    /// Get HEAD checkpoint
    pub async fn get_head(&mut self) -> Result<Option<Checkpoint>> {
        match self.send_request(&IpcRequest::GetHead).await? {
            IpcResponse::Head(checkpoint) => Ok(checkpoint),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetHead"),
        }
    }

    /// Flush pending changes and create checkpoint immediately
    /// Returns the checkpoint ID if created, None if nothing to checkpoint
    pub async fn flush_checkpoint(&mut self) -> Result<Option<String>> {
        match self.send_request(&IpcRequest::FlushCheckpoint).await? {
            IpcResponse::CheckpointFlushed(checkpoint_id) => Ok(checkpoint_id),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to FlushCheckpoint"),
        }
    }

    /// Get checkpoints with pagination (for log command)
    pub async fn get_checkpoints(&mut self, limit: Option<usize>, offset: Option<usize>) -> Result<Vec<Checkpoint>> {
        let request = IpcRequest::GetCheckpoints { limit, offset };
        match self.send_request(&request).await? {
            IpcResponse::Checkpoints(checkpoints) => Ok(checkpoints),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetCheckpoints"),
        }
    }

    /// Get total checkpoint count (cached for performance)
    pub async fn get_checkpoint_count(&mut self) -> Result<usize> {
        match self.send_request(&IpcRequest::GetCheckpointCount).await? {
            IpcResponse::CheckpointCount(count) => Ok(count),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetCheckpointCount"),
        }
    }

    /// Get multiple checkpoints in one IPC call (for diff command)
    pub async fn get_checkpoint_batch(&mut self, ids: Vec<String>) -> Result<Vec<Option<Checkpoint>>> {
        match self.send_request(&IpcRequest::GetCheckpointBatch(ids)).await? {
            IpcResponse::CheckpointBatch(checkpoints) => Ok(checkpoints),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetCheckpointBatch"),
        }
    }

    /// Get full status information in one IPC call (for status command)
    pub async fn get_status_full(&mut self) -> Result<(DaemonStatus, Option<Checkpoint>, usize)> {
        match self.send_request(&IpcRequest::GetStatusFull).await? {
            IpcResponse::StatusFull { status, head, checkpoint_count } => {
                Ok((status, head, checkpoint_count))
            }
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetStatusFull"),
        }
    }

    /// Get checkpoint count and list in one IPC call (for log command)
    pub async fn get_log_data(&mut self, limit: Option<usize>, offset: Option<usize>) -> Result<(usize, Vec<Checkpoint>)> {
        let request = IpcRequest::GetLogData { limit, offset };
        match self.send_request(&request).await? {
            IpcResponse::LogData { count, checkpoints } => Ok((count, checkpoints)),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetLogData"),
        }
    }

    /// Resolve checkpoint references (supports full IDs, short prefixes, pin names)
    pub async fn resolve_checkpoint_refs(&mut self, refs: Vec<String>) -> Result<Vec<Option<Checkpoint>>> {
        let request = IpcRequest::ResolveCheckpointRefs(refs);
        match self.send_request(&request).await? {
            IpcResponse::ResolvedCheckpoints(checkpoints) => Ok(checkpoints),
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to ResolveCheckpointRefs"),
        }
    }

    /// Get repository info data (for info command)
    pub async fn get_info_data(&mut self) -> Result<(usize, Vec<String>, u64)> {
        let request = IpcRequest::GetInfoData;
        match self.send_request(&request).await? {
            IpcResponse::InfoData { total_checkpoints, checkpoint_ids, store_size_bytes } => {
                Ok((total_checkpoints, checkpoint_ids, store_size_bytes))
            }
            IpcResponse::Error(err) => anyhow::bail!("Daemon error: {}", err),
            _ => anyhow::bail!("Unexpected response to GetInfoData"),
        }
    }
}

/// Resilient IPC client with automatic retry and exponential backoff
pub struct ResilientIpcClient {
    socket_path: std::path::PathBuf,
    max_retries: usize,
    initial_backoff: Duration,
}

impl ResilientIpcClient {
    pub fn new(socket_path: std::path::PathBuf) -> Self {
        Self {
            socket_path,
            max_retries: 10,
            initial_backoff: Duration::from_millis(50),
        }
    }

    /// Connect with retry and exponential backoff
    pub async fn connect_with_retry(&self) -> Result<IpcClient> {
        let mut backoff = self.initial_backoff;

        for attempt in 0..self.max_retries {
            match IpcClient::connect(&self.socket_path).await {
                Ok(client) => {
                    if attempt > 0 {
                        tracing::debug!("Connected after {} retries", attempt);
                    }
                    return Ok(client);
                }
                Err(e) => {
                    if attempt == self.max_retries - 1 {
                        // Final attempt failed
                        anyhow::bail!(
                            "Failed to connect to daemon after {} attempts: {}",
                            self.max_retries,
                            e
                        );
                    }

                    // Retry with exponential backoff
                    tracing::debug!(
                        "Connection attempt {} failed, retrying in {:?}",
                        attempt + 1,
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.mul_f32(1.5).min(Duration::from_secs(1));
                }
            }
        }

        unreachable!()
    }

    /// Send request with automatic reconnect on failure
    pub async fn send_request_resilient(&self, request: &IpcRequest) -> Result<IpcResponse> {
        let mut client = self.connect_with_retry().await?;
        client.send_request(request).await
    }
}

/// IPC server for daemon to handle CLI requests
pub struct IpcServer {
    listener: UnixListener,
}

impl IpcServer {
    /// Start IPC server on Unix socket
    pub async fn start(socket_path: &Path) -> Result<Self> {
        // Remove stale socket if exists
        if socket_path.exists() {
            std::fs::remove_file(socket_path)
                .context("Failed to remove stale socket")?;
        }

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create socket directory")?;
        }

        // Bind Unix socket
        let listener = UnixListener::bind(socket_path)
            .context("Failed to bind Unix socket")?;

        // Set socket permissions to owner-only (0600)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(socket_path, permissions)
                .context("Failed to set socket permissions")?;
        }

        Ok(Self { listener })
    }

    /// Accept a single connection and return the stream
    pub async fn accept(&self) -> Result<UnixStream> {
        let (stream, _addr) = self
            .listener
            .accept()
            .await
            .context("Failed to accept connection")?;
        Ok(stream)
    }
}

/// Handle a single IPC connection
pub async fn handle_connection<F, Fut>(mut stream: UnixStream, handler: F) -> Result<()>
where
    F: FnOnce(IpcRequest) -> Fut,
    Fut: std::future::Future<Output = Result<IpcResponse>>,
{
    // Read length prefix (4 bytes)
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("Failed to read request length")?;

    let len = u32::from_le_bytes(len_buf) as usize;

    // Sanity check (prevent DoS)
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!("IPC message too large: {} bytes", len);
    }

    // Read payload
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .context("Failed to read request payload")?;

    // Deserialize request
    let request: IpcRequest = bincode::deserialize(&payload)
        .context("Failed to deserialize request")?;

    // Process request
    let response = handler(request).await?;

    // Serialize response
    let response_bytes = bincode::serialize(&response)
        .context("Failed to serialize response")?;

    if response_bytes.len() > MAX_MESSAGE_SIZE {
        anyhow::bail!("Response too large: {} bytes", response_bytes.len());
    }

    let response_len = (response_bytes.len() as u32).to_le_bytes();

    // Write response
    stream
        .write_all(&response_len)
        .await
        .context("Failed to write response length")?;

    stream
        .write_all(&response_bytes)
        .await
        .context("Failed to write response payload")?;

    stream
        .flush()
        .await
        .context("Failed to flush response")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let request = IpcRequest::GetStatus;
        let serialized = bincode::serialize(&request).unwrap();
        let deserialized: IpcRequest = bincode::deserialize(&serialized).unwrap();

        matches!(deserialized, IpcRequest::GetStatus);
    }

    #[test]
    fn test_response_serialization() {
        let status = DaemonStatus {
            running: true,
            pid: 12345,
            start_time_ms: 1704067200000, // Fixed timestamp for testing
            checkpoints_created: 42,
            last_checkpoint_time: Some(1234567890),
            watcher_paths: 100,
            checkpoints_skipped: 0,
        };

        let response = IpcResponse::Status(status.clone());
        let serialized = bincode::serialize(&response).unwrap();
        let deserialized: IpcResponse = bincode::deserialize(&serialized).unwrap();

        if let IpcResponse::Status(s) = deserialized {
            assert_eq!(s.pid, status.pid);
            assert_eq!(s.start_time_ms, status.start_time_ms);
        } else {
            panic!("Expected Status response");
        }
    }
}
