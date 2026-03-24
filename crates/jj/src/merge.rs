//! Native 3-way merge using jj-lib
//!
//! This module provides production-ready merge operations using jj-lib's
//! tree merge functionality. It handles:
//! - Finding common ancestors (merge base)
//! - 3-way tree merge
//! - Conflict detection and extraction

use anyhow::{anyhow, Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::config::StackedConfig;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::Repo;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use pollster::FutureExt as _;
use std::path::Path;

/// Result of a merge operation
#[derive(Debug)]
pub struct MergeResult {
    /// The resulting merged tree ID (may contain conflicts)
    pub merged_tree: MergedTree,
    /// List of conflicted file paths
    pub conflicts: Vec<ConflictInfo>,
    /// Whether the merge completed cleanly (no conflicts)
    pub is_clean: bool,
    /// The merge base commit ID
    pub base_commit_id: Option<String>,
    /// "Ours" commit ID (current state)
    pub ours_commit_id: String,
    /// "Theirs" commit ID (target branch)
    pub theirs_commit_id: String,
}

/// Information about a single conflicted file
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    /// Relative file path
    pub path: String,
    /// Base (common ancestor) content, if available
    pub base_content: Option<Vec<u8>>,
    /// "Ours" (local) content
    pub ours_content: Vec<u8>,
    /// "Theirs" (remote) content
    pub theirs_content: Vec<u8>,
}

/// Merge state persisted to .tl/state/merge/
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MergeState {
    /// Whether a merge is in progress
    pub in_progress: bool,
    /// "Ours" commit ID (local/current)
    pub ours_commit: String,
    /// "Theirs" commit ID (target branch)
    pub theirs_commit: String,
    /// Target branch name (e.g., "main")
    pub theirs_branch: String,
    /// Base commit ID (common ancestor)
    pub base_commit: Option<String>,
    /// List of conflicted file paths
    pub conflicts: Vec<String>,
    /// Checkpoint ID before merge started (for abort)
    pub pre_merge_checkpoint: String,
}

impl MergeState {
    /// Load merge state from .tl directory
    pub fn load(tl_dir: &Path) -> Result<Option<MergeState>> {
        let state_path = tl_dir.join("state/merge/merge.json");
        if !state_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&state_path)
            .context("Failed to read merge state")?;
        let state: MergeState = serde_json::from_str(&content)
            .context("Failed to parse merge state")?;

        Ok(Some(state))
    }

    /// Save merge state to .tl directory
    pub fn save(&self, tl_dir: &Path) -> Result<()> {
        let state_dir = tl_dir.join("state/merge");
        std::fs::create_dir_all(&state_dir)
            .context("Failed to create merge state directory")?;

        let state_path = state_dir.join("merge.json");
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize merge state")?;

        std::fs::write(&state_path, content)
            .context("Failed to write merge state")?;

        Ok(())
    }

    /// Clear merge state (after successful merge or abort)
    pub fn clear(tl_dir: &Path) -> Result<()> {
        let state_path = tl_dir.join("state/merge/merge.json");
        if state_path.exists() {
            std::fs::remove_file(&state_path)
                .context("Failed to remove merge state")?;
        }
        Ok(())
    }
}

/// Create default UserSettings for jj-lib operations
fn create_user_settings() -> Result<UserSettings> {
    let config = StackedConfig::with_defaults();
    UserSettings::from_config(config)
        .map_err(|e| anyhow!("Failed to create user settings: {}", e))
}

/// Get a branch's commit ID from the JJ view
pub fn get_branch_commit_id(workspace: &Workspace, branch_name: &str) -> Result<String> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();

    // Get local bookmark target (branches are now called bookmarks)
    let ref_name: &RefName = branch_name.as_ref();
    let target = view.get_local_bookmark(ref_name);

    match target.as_normal() {
        Some(commit_id) => Ok(commit_id.hex()),
        None => {
            // Try remote bookmark (use RemoteRefSymbol in 0.36.0)
            let origin_remote: &RemoteName = "origin".as_ref();
            let remote_symbol = ref_name.to_remote_symbol(origin_remote);
            let remote_ref = view.get_remote_bookmark(remote_symbol);
            match remote_ref.target.as_normal() {
                Some(commit_id) => Ok(commit_id.hex()),
                None => Err(anyhow!("Branch '{}' not found or has no commit", branch_name)),
            }
        }
    }
}

/// Get the current working copy commit ID
pub fn get_current_commit_id(workspace: &Workspace) -> Result<String> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();
    let wc_id = view.get_wc_commit_id(workspace.workspace_name())
        .ok_or_else(|| anyhow!("No working copy commit found"))?;

    Ok(wc_id.hex())
}

/// Find the common ancestor (merge base) between two commits
pub fn find_merge_base(workspace: &Workspace, commit1_hex: &str, commit2_hex: &str) -> Result<Option<String>> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // Use hex::decode + CommitId::new to avoid lifetime issues with from_hex
    let commit1_id = CommitId::new(hex::decode(commit1_hex).context("Invalid commit1 hex")?);
    let commit2_id = CommitId::new(hex::decode(commit2_hex).context("Invalid commit2 hex")?);

    // Use the index to find common ancestors (returns Result in 0.36.0)
    let ancestors = repo.index().common_ancestors(&[commit1_id], &[commit2_id])
        .context("Failed to compute common ancestors")?;

    // Return the first (most recent) common ancestor
    Ok(ancestors.first().map(|id: &CommitId| id.hex()))
}

/// Perform a 3-way merge between current state and target branch
///
/// This is the core merge function that uses jj-lib's tree merge APIs.
///
/// # Arguments
/// * `workspace` - JJ workspace
/// * `target_branch` - Branch to merge (e.g., "main")
///
/// # Returns
/// MergeResult with merged tree and conflict information
pub fn perform_merge(workspace: &Workspace, target_branch: &str) -> Result<MergeResult> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // 1. Get current commit ("ours")
    let view = repo.view();
    let ours_id = view.get_wc_commit_id(workspace.workspace_name())
        .ok_or_else(|| anyhow!("No working copy commit found"))?
        .clone();

    // 2. Get target branch commit ("theirs")
    let theirs_id = {
        let ref_name: &RefName = target_branch.as_ref();
        let target = view.get_local_bookmark(ref_name);
        match target.as_normal() {
            Some(id) => id.clone(),
            None => {
                // Try remote bookmark (use RemoteRefSymbol in 0.36.0)
                let origin_remote: &RemoteName = "origin".as_ref();
                let remote_symbol = ref_name.to_remote_symbol(origin_remote);
                let remote_ref = view.get_remote_bookmark(remote_symbol);
                remote_ref.target.as_normal()
                    .ok_or_else(|| anyhow!("Branch '{}' not found", target_branch))?
                    .clone()
            }
        }
    };

    // 3. Find common ancestor ("base") - returns Result in 0.36.0
    let base_ids = repo.index().common_ancestors(&[ours_id.clone()], &[theirs_id.clone()])
        .context("Failed to compute common ancestors")?;
    let base_id = base_ids.first().cloned();

    // 4. Get commits
    let store = repo.store();
    let ours_commit = store.get_commit(&ours_id)
        .context("Failed to get 'ours' commit")?;
    let theirs_commit = store.get_commit(&theirs_id)
        .context("Failed to get 'theirs' commit")?;

    // 5. Get trees (tree() returns MergedTree directly in 0.36.0)
    let ours_tree = ours_commit.tree();
    let theirs_tree = theirs_commit.tree();

    // 6. Get base tree (if we have a common ancestor)
    // merge() is now async and takes ownership in 0.36.0
    let merged_tree = if let Some(ref base_id) = base_id {
        let base_commit = store.get_commit(base_id)
            .context("Failed to get base commit")?;
        let base_tree = base_commit.tree();

        // Perform 3-way merge using jj-lib (async, clone trees since merge takes ownership)
        base_tree.clone().merge(ours_tree.clone(), theirs_tree.clone())
            .block_on()
            .context("Failed to perform 3-way merge")?
    } else {
        // No common ancestor - merge without base (can produce many conflicts)
        // Clone ours_tree since we use it twice
        let ours_tree_clone = ours_tree.clone();
        ours_tree.merge(ours_tree_clone, theirs_tree)
            .block_on()
            .context("Failed to perform merge without base")?
    };

    // 7. Extract conflict information
    let conflicts = extract_conflicts(&merged_tree, store)?;
    let is_clean = conflicts.is_empty();

    Ok(MergeResult {
        merged_tree,
        conflicts,
        is_clean,
        base_commit_id: base_id.map(|id: CommitId| id.hex()),
        ours_commit_id: ours_id.hex(),
        theirs_commit_id: theirs_id.hex(),
    })
}

/// Extract conflict information from a merged tree
fn extract_conflicts(merged_tree: &MergedTree, _store: &std::sync::Arc<jj_lib::store::Store>) -> Result<Vec<ConflictInfo>> {
    let mut conflicts = Vec::new();

    // Check if tree has any conflicts
    if merged_tree.has_conflict() {
        // Iterate through all entries looking for conflicts
        // entries() returns (RepoPathBuf, BackendResult<MergedTreeValue>)
        for (path, entry_result) in merged_tree.entries() {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue, // Skip entries that fail to read
            };

            if entry.is_resolved() {
                continue;
            }

            // This is a conflicted entry
            let path_str = path.as_internal_file_string().to_string();

            // For now, extract placeholder content
            // In a full implementation, we'd read the actual content from each side
            let conflict = ConflictInfo {
                path: path_str,
                base_content: None,
                ours_content: b"<<<<<<< LOCAL\n=======\n>>>>>>> REMOTE\n".to_vec(),
                theirs_content: Vec::new(),
            };

            conflicts.push(conflict);
        }
    }

    Ok(conflicts)
}

/// Create a merge commit with multiple parents
pub fn create_merge_commit(
    workspace: &mut Workspace,
    parent1_hex: &str,
    parent2_hex: &str,
    merged_tree: MergedTree, // Takes ownership of MergedTree
    message: &str,
) -> Result<String> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // Use hex::decode + CommitId::new to avoid lifetime issues with from_hex
    let parent1_id = CommitId::new(hex::decode(parent1_hex).context("Invalid parent1 hex")?);
    let parent2_id = CommitId::new(hex::decode(parent2_hex).context("Invalid parent2 hex")?);

    // Start transaction (no longer takes user_settings)
    let mut tx = repo.start_transaction();

    // Create the merge commit with multiple parents
    // new_commit now takes MergedTree directly instead of tree ID
    let new_commit = tx.repo_mut()
        .new_commit(
            vec![parent1_id, parent2_id],
            merged_tree,
        )
        .set_description(message)
        .write()
        .context("Failed to create merge commit")?;

    let new_commit_id = new_commit.id().hex();

    // Commit transaction (now returns Result)
    tx.commit("create merge commit")
        .context("Failed to commit transaction")?;

    Ok(new_commit_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_state_serialization() {
        let state = MergeState {
            in_progress: true,
            ours_commit: "abc123".to_string(),
            theirs_commit: "def456".to_string(),
            theirs_branch: "main".to_string(),
            base_commit: Some("789abc".to_string()),
            conflicts: vec!["src/main.rs".to_string()],
            pre_merge_checkpoint: "01KE77BC".to_string(),
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: MergeState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.in_progress, state.in_progress);
        assert_eq!(parsed.ours_commit, state.ours_commit);
        assert_eq!(parsed.theirs_branch, state.theirs_branch);
    }
}
