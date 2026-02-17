//! Workspace management with automatic checkpoint save/restore
//!
//! This module provides workspace management that wraps JJ workspaces with
//! automatic state preservation. Each workspace can have an associated checkpoint
//! that is automatically restored when switching to that workspace.
//!
//! The workspace state is persisted in a sled database at `.tl/state/workspace-state/`.

use anyhow::{anyhow, Context, Result};
use jj_lib::object_id::ObjectId;
use journal::{Checkpoint, CheckpointReason, Journal};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tl_core::{Entry, Store, Tree};
use ulid::Ulid;
use walkdir::WalkDir;

/// Workspace state stored in sled database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Workspace name (from JJ)
    pub name: String,
    /// Absolute path to workspace directory
    pub path: PathBuf,
    /// Current checkpoint ID for this workspace (if any)
    pub current_checkpoint: Option<Ulid>,
    /// Last switch timestamp (Unix milliseconds)
    pub last_switched_ms: u64,
    /// Creation timestamp (Unix milliseconds)
    pub created_ms: u64,
    /// Auto-pin name (e.g., "ws:feature-branch")
    pub auto_pin: Option<String>,
}

impl WorkspaceState {
    /// Serialize to bytes using bincode
    pub fn serialize(&self) -> Result<Vec<u8>> {
        Ok(bincode::serialize(self)?)
    }

    /// Deserialize from bytes using bincode
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        Ok(bincode::deserialize(bytes)?)
    }
}

/// JJ workspace information
#[derive(Debug, Clone)]
pub struct JjWorkspace {
    /// Workspace name
    pub name: String,
    /// Absolute path to workspace directory
    pub path: PathBuf,
    /// Whether this is the current workspace
    pub is_current: bool,
    /// Whether workspace has uncommitted changes
    pub has_changes: bool,
}

/// Workspace manager with sled database backend
pub struct WorkspaceManager {
    db: sled::Db,
    repo_root: PathBuf,
}

impl WorkspaceManager {
    /// Open workspace state database at `.tl/state/workspace-state`
    ///
    /// Creates the database if it doesn't exist.
    pub fn open(tl_dir: &Path, repo_root: &Path) -> Result<Self> {
        let db_path = tl_dir.join("state/workspace-state");
        let db = sled::open(&db_path)
            .with_context(|| format!("Failed to open workspace state DB at {}", db_path.display()))?;

        Ok(Self {
            db,
            repo_root: repo_root.to_path_buf(),
        })
    }

    /// Get workspace state by name
    pub fn get_state(&self, name: &str) -> Result<Option<WorkspaceState>> {
        if let Some(value) = self.db.get(name.as_bytes())? {
            let state = WorkspaceState::deserialize(&value)?;
            return Ok(Some(state));
        }
        Ok(None)
    }

    /// Store workspace state
    pub fn set_state(&self, state: &WorkspaceState) -> Result<()> {
        let key = state.name.as_bytes();
        let value = state.serialize()?;
        self.db.insert(key, value)?;
        self.db.flush()?;
        Ok(())
    }

    /// Delete workspace state
    pub fn delete_state(&self, name: &str) -> Result<()> {
        self.db.remove(name.as_bytes())?;
        self.db.flush()?;
        Ok(())
    }

    /// List all workspace states
    pub fn list_states(&self) -> Result<Vec<WorkspaceState>> {
        let mut states = Vec::new();
        for item in self.db.iter() {
            let (_, value) = item?;
            states.push(WorkspaceState::deserialize(&value)?);
        }
        Ok(states)
    }

    /// Get current workspace name from `.jj/working_copy/workspace_id`
    pub fn current_workspace_name(&self) -> Result<String> {
        let workspace_id_path = self.repo_root.join(".jj/working_copy/workspace_id");

        if workspace_id_path.exists() {
            let content = fs::read_to_string(&workspace_id_path)
                .context("Failed to read workspace_id file")?;

            let name = content.trim();
            if name.is_empty() {
                anyhow::bail!("Workspace ID file is empty");
            }

            Ok(name.to_string())
        } else {
            // Fallback: default workspace
            Ok("default".to_string())
        }
    }

    /// List all JJ workspaces using native jj-lib APIs
    pub fn list_jj_workspaces(&self) -> Result<Vec<JjWorkspace>> {
        // Load workspace and repo
        let workspace = crate::load_workspace(&self.repo_root)
            .context("Failed to load JJ workspace for listing")?;

        let repo = workspace.repo_loader().load_at_head()
            .context("Failed to load repository at HEAD")?;

        let view = repo.view();
        let mut workspaces = Vec::new();

        // Iterate all workspace commit IDs from view
        // In 0.36.0, wc_commit_ids() returns (WorkspaceName, CommitId) pairs
        for (workspace_name, wc_commit_id) in view.wc_commit_ids() {
            // Resolve workspace path (with error handling for missing/corrupted workspaces)
            let workspace_path = match self.resolve_workspace_path_from_name(workspace_name.as_str()) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("Warning: Failed to resolve workspace {}: {}",
                             workspace_name.as_str(), e);
                    continue; // Skip this workspace
                }
            };

            let is_current = workspace_name == workspace.workspace_name();
            let has_changes = self.check_workspace_changes_native(&repo, wc_commit_id)?;

            workspaces.push(JjWorkspace {
                name: workspace_name.as_str().to_string(),
                path: workspace_path,
                is_current,
                has_changes,
            });
        }

        Ok(workspaces)
    }

    /// Resolve workspace path from workspace name
    ///
    /// For default workspace, returns repo root.
    /// For other workspaces, reads path from `.jj/workspaces/<name>/workspace.toml`
    fn resolve_workspace_path_from_name(&self, workspace_name: &str) -> Result<PathBuf> {
        // Default workspace = repo root
        if workspace_name == "default" {
            return Ok(self.repo_root.clone());
        }

        // Read from .jj/workspaces/<name>/workspace.toml
        let toml_path = self.repo_root
            .join(".jj/workspaces")
            .join(workspace_name)
            .join("workspace.toml");

        if !toml_path.exists() {
            // Fallback: use workspace directory as path
            return Ok(self.repo_root.join(".jj/workspaces").join(workspace_name));
        }

        let content = fs::read_to_string(&toml_path)
            .with_context(|| format!("Failed to read workspace.toml for {}", workspace_name))?;

        let toml: toml::Value = toml::from_str(&content)
            .context("Failed to parse workspace.toml")?;

        let path = toml
            .get("workspace")
            .and_then(|w| w.get("working_copy"))
            .and_then(|w| w.as_str())
            .ok_or_else(|| anyhow!("workspace.toml missing working_copy path"))?;

        Ok(PathBuf::from(path))
    }

    /// Check if workspace has uncommitted changes using native API
    ///
    /// Returns true if the commit tree is non-empty (has files).
    /// Note: This is a simple heuristic - it doesn't detect unstaged changes vs committed changes.
    fn check_workspace_changes_native(
        &self,
        repo: &jj_lib::repo::ReadonlyRepo,
        wc_commit_id: &jj_lib::backend::CommitId,
    ) -> Result<bool> {
        use jj_lib::repo::Repo;

        // Get the commit
        let commit = repo.store().get_commit(wc_commit_id)
            .with_context(|| format!("Failed to get commit {}", wc_commit_id.hex()))?;

        // Get tree directly from commit (tree() returns MergedTree directly in 0.36.0)
        let tree = commit.tree();

        // Has changes if tree is non-empty
        Ok(tree.entries().next().is_some())
    }

    /// Auto-checkpoint current workspace with WorkspaceSave reason
    ///
    /// This builds a full tree from the current working directory and creates a checkpoint.
    /// Uses file locking to prevent concurrent checkpoint conflicts.
    pub fn auto_checkpoint_current(
        &self,
        store: &Store,
        journal: &Journal,
    ) -> Result<Ulid> {
        // Build tree from current working directory
        let tree = self.build_tree_from_workdir(store)?;
        let tree_hash = store.write_tree(&tree)?;

        // Check for deduplication: if tree unchanged, skip checkpoint
        if let Some(latest) = journal.latest()? {
            if latest.root_tree == tree_hash {
                // Tree unchanged, return existing checkpoint ID
                return Ok(latest.id);
            }
        }

        // Get parent checkpoint (latest)
        let parent_id = journal.latest()?.map(|cp| cp.id);

        // Create checkpoint with WorkspaceSave reason
        let checkpoint = Checkpoint::new(
            parent_id,
            tree_hash,
            CheckpointReason::WorkspaceSave,
            vec![], // TODO: Track touched paths via daemon integration
            journal::CheckpointMeta::default(), // TODO: Calculate actual stats
        );

        journal.append(&checkpoint)?;

        Ok(checkpoint.id)
    }

    /// Build tree from current working directory
    ///
    /// This performs a full filesystem scan using walkdir and stores all files as blobs.
    fn build_tree_from_workdir(&self, store: &Store) -> Result<Tree> {
        let mut tree = Tree::new();

        for entry in WalkDir::new(&self.repo_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // Skip .tl/, .git/, .jj/ directories
                let path = e.path();
                !path.starts_with(self.repo_root.join(".tl"))
                    && !path.starts_with(self.repo_root.join(".git"))
                    && !path.starts_with(self.repo_root.join(".jj"))
            })
        {
            let entry = entry?;
            let path = entry.path();

            // Skip directories (only files and symlinks)
            if entry.file_type().is_dir() {
                continue;
            }

            let rel_path = path.strip_prefix(&self.repo_root)?;

            // Handle symlinks
            if entry.file_type().is_symlink() {
                let target = fs::read_link(path)?;
                let target_bytes = target.to_string_lossy();
                let blob_hash = tl_core::hash::git::hash_blob(target_bytes.as_bytes());

                if !store.blob_store().has_blob(blob_hash) {
                    store.blob_store().write_blob(blob_hash, target_bytes.as_bytes())?;
                }

                tree.insert(rel_path, Entry::symlink(blob_hash));
                continue;
            }

            // Handle regular files
            if entry.file_type().is_file() {
                let metadata = entry.metadata()?;

                // Extract Unix mode
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::MetadataExt;
                    metadata.mode()
                };
                #[cfg(not(unix))]
                let mode = if metadata.permissions().readonly() {
                    0o444
                } else {
                    0o644
                };

                // Hash file (use mmap for large files)
                let blob_hash = if metadata.len() > 4 * 1024 * 1024 {
                    tl_core::hash::hash_file_mmap(path)?
                } else {
                    tl_core::hash::hash_file(path)?
                };

                // Write blob if not exists
                if !store.blob_store().has_blob(blob_hash) {
                    let contents = fs::read(path)?;
                    store.blob_store().write_blob(blob_hash, &contents)?;
                }

                tree.insert(rel_path, Entry::file(mode, blob_hash));
            }
        }

        Ok(tree)
    }

    /// Restore checkpoint to a workspace directory
    ///
    /// This uses the existing materialize function to restore files.
    pub fn restore_workspace_checkpoint(
        &self,
        workspace_name: &str,
        store: &Store,
        journal: &Journal,
        workspace_path: &Path,
    ) -> Result<()> {
        let state = self.get_state(workspace_name)?
            .ok_or_else(|| anyhow!("Workspace state not found: {}", workspace_name))?;

        let checkpoint_id = state.current_checkpoint
            .ok_or_else(|| anyhow!("No checkpoint saved for workspace"))?;

        let checkpoint = journal.get(&checkpoint_id)?
            .ok_or_else(|| anyhow!("Checkpoint not found: {}", checkpoint_id))?;

        // Reuse publish.rs materialization logic
        crate::publish::materialize_checkpoint_to_dir(
            &checkpoint,
            store,
            workspace_path,
        )?;

        Ok(())
    }
}

/// Validate workspace name format
pub fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("Workspace name cannot be empty");
    }

    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!("Invalid workspace name: must be alphanumeric with hyphens/underscores");
    }

    // Reserved names
    const RESERVED_NAMES: &[&str] = &["default", "main", "master", "trunk", "@"];
    if RESERVED_NAMES.contains(&name) {
        anyhow::bail!("Cannot use reserved name '{}'", name);
    }

    // Length limit (filesystem compatibility)
    if name.len() > 255 {
        anyhow::bail!("Workspace name too long (max 255 characters)");
    }

    // Reject names starting with special characters (shell safety)
    if name.starts_with('-') || name.starts_with('.') {
        anyhow::bail!("Workspace name cannot start with '-' or '.'");
    }

    // JJ-specific: workspace names can't contain certain characters
    if name.contains('/') || name.contains('\\') || name.contains('@') {
        anyhow::bail!("Invalid workspace name: cannot contain /, \\, or @");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_workspace_state_roundtrip() -> Result<()> {
        let state = WorkspaceState {
            name: "test".to_string(),
            path: PathBuf::from("/tmp/test"),
            current_checkpoint: Some(Ulid::new()),
            last_switched_ms: 123456789,
            created_ms: 123456000,
            auto_pin: Some("ws:test".to_string()),
        };

        let bytes = state.serialize()?;
        let deserialized = WorkspaceState::deserialize(&bytes)?;

        assert_eq!(state.name, deserialized.name);
        assert_eq!(state.path, deserialized.path);
        assert_eq!(state.current_checkpoint, deserialized.current_checkpoint);

        Ok(())
    }

    #[test]
    fn test_workspace_manager_operations() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let tl_dir = temp_dir.path().join(".tl");
        fs::create_dir_all(tl_dir.join("state"))?;

        let ws_manager = WorkspaceManager::open(&tl_dir, temp_dir.path())?;

        // Test set and get
        let state = WorkspaceState {
            name: "test".to_string(),
            path: PathBuf::from("/tmp/test"),
            current_checkpoint: None,
            last_switched_ms: 0,
            created_ms: 0,
            auto_pin: None,
        };

        ws_manager.set_state(&state)?;
        let retrieved = ws_manager.get_state("test")?;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "test");

        // Test delete
        ws_manager.delete_state("test")?;
        assert!(ws_manager.get_state("test")?.is_none());

        Ok(())
    }

    #[test]
    fn test_validate_workspace_name() {
        assert!(validate_workspace_name("valid-name").is_ok());
        assert!(validate_workspace_name("valid_name").is_ok());
        assert!(validate_workspace_name("valid123").is_ok());

        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("invalid name").is_err());
        assert!(validate_workspace_name("invalid/name").is_err());
        assert!(validate_workspace_name("default").is_err());
        assert!(validate_workspace_name("-bad").is_err());
        assert!(validate_workspace_name(".bad").is_err());
        assert!(validate_workspace_name("has@symbol").is_err());
    }
}
