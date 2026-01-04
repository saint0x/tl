//! IPC between CLI and daemon using Unix sockets

use anyhow::{Context, Result};
use journal::Checkpoint;
use serde::{Deserialize, Serialize};
use std::path::Path;
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
    /// Uptime in seconds
    pub uptime_secs: u64,
    /// Total checkpoints created by this daemon instance
    pub checkpoints_created: u64,
    /// Timestamp of last checkpoint (ms since epoch)
    pub last_checkpoint_time: Option<u64>,
    /// Number of paths currently being watched
    pub watcher_paths: usize,
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
            uptime_secs: 300,
            checkpoints_created: 42,
            last_checkpoint_time: Some(1234567890),
            watcher_paths: 100,
        };

        let response = IpcResponse::Status(status.clone());
        let serialized = bincode::serialize(&response).unwrap();
        let deserialized: IpcResponse = bincode::deserialize(&serialized).unwrap();

        if let IpcResponse::Status(s) = deserialized {
            assert_eq!(s.pid, status.pid);
            assert_eq!(s.uptime_secs, status.uptime_secs);
        } else {
            panic!("Expected Status response");
        }
    }
}
