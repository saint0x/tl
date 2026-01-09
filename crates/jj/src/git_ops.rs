//! Native Git operations using jj-lib's high-level APIs
//!
//! This module provides production-ready git push/fetch operations
//! using jj-lib's native functions that use git subprocess internally.
//! Authentication is handled by the real git binary, which properly
//! supports all credential helpers (gh auth, osxkeychain, SSH agent, etc.)

use anyhow::{anyhow, Context, Result};
use jj_lib::config::StackedConfig;
use jj_lib::git::{
    expand_fetch_refspecs, push_branches, GitBranchPushTargets, GitFetch, GitFetchError,
    GitPushError, GitSettings, RemoteCallbacks,
};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::Repo;
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;
use jj_lib::workspace::Workspace;

/// Create default UserSettings for jj-lib operations
fn create_user_settings() -> Result<UserSettings> {
    let config = StackedConfig::with_defaults();
    UserSettings::from_config(config)
        .map_err(|e| anyhow!("Failed to create user settings: {}", e))
}

/// Result of a push operation for a single branch
#[derive(Debug, Clone)]
pub struct BranchPushResult {
    pub name: String,
    pub status: BranchPushStatus,
    pub old_commit: Option<String>,
    pub new_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BranchPushStatus {
    /// Successfully pushed
    Pushed,
    /// Already up to date
    UpToDate,
    /// Rejected - remote has diverged, needs --force
    Diverged,
    /// Rejected for other reasons
    Rejected(String),
    /// Skipped (no change)
    Skipped,
}

/// Push to Git remote using jj-lib's native push_branches API
///
/// This uses JJ's high-level push function which handles:
/// - Exporting JJ refs to Git
/// - Pushing to remote via git subprocess (handles all auth automatically)
/// - Updating remote tracking branches
///
/// Returns detailed results per branch.
///
/// # Arguments
/// * `workspace` - JJ workspace (must be git-backed)
/// * `bookmark` - Optional bookmark name (standard Git branch name)
/// * `all` - Push all bookmarks
/// * `force` - Force push (non-fast-forward)
pub fn native_git_push(
    workspace: &mut Workspace,
    bookmark: Option<&str>,
    all: bool,
    _force: bool,
) -> Result<Vec<BranchPushResult>> {
    // Load repo at HEAD
    let user_settings = create_user_settings()?;
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // Start transaction
    let mut tx = repo.start_transaction();

    // Get GitSettings for subprocess operations
    let git_settings = GitSettings::from_settings(&user_settings)
        .context("Failed to get git settings")?;

    // Get branches (bookmarks) to push from view
    let view = tx.repo_mut().view();

    // Collect branches to validate and push
    let mut branches_to_push: Vec<(String, Option<String>, Option<String>)> = Vec::new(); // (name, local_commit, remote_commit)

    // Create RemoteName for "origin" (used for remote bookmark lookups)
    let origin_remote: &RemoteName = "origin".as_ref();

    if all {
        // Collect all bookmarks
        for (bookmark_name, target) in view.local_bookmarks() {
            if let Some(local_commit_id) = target.as_normal() {
                // Create remote symbol for lookup
                let remote_symbol = bookmark_name.to_remote_symbol(origin_remote);
                let remote_ref = view.get_remote_bookmark(remote_symbol);
                let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());
                branches_to_push.push((
                    bookmark_name.as_str().to_string(),
                    Some(local_commit_id.hex()),
                    remote_commit_id,
                ));
            }
        }
    } else if let Some(bookmark_name) = bookmark {
        // Single bookmark (use name as-is, standard Git naming)
        let full_name = bookmark_name.to_string();

        // Convert to RefName for lookup
        let ref_name: &RefName = full_name.as_ref();
        let target = view.get_local_bookmark(ref_name);
        if let Some(local_commit_id) = target.as_normal() {
            let remote_symbol = ref_name.to_remote_symbol(origin_remote);
            let remote_ref = view.get_remote_bookmark(remote_symbol);
            let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());
            branches_to_push.push((
                full_name.clone(),
                Some(local_commit_id.hex()),
                remote_commit_id,
            ));
        } else {
            anyhow::bail!("Branch {} not found or not at a single commit", full_name);
        }
    } else {
        // Auto-detect: find main or first available bookmark (like `git push` auto-detects current branch)
        let mut auto_bookmark: Option<String> = None;

        // Priority 1: main (default branch)
        let main_ref: &RefName = "main".as_ref();
        if view.get_local_bookmark(main_ref).is_present() {
            auto_bookmark = Some("main".to_string());
        } else {
            // Priority 2: master (alternative default)
            let master_ref: &RefName = "master".as_ref();
            if view.get_local_bookmark(master_ref).is_present() {
                auto_bookmark = Some("master".to_string());
            } else {
                // Priority 3: First available bookmark
                for (bookmark_name, target) in view.local_bookmarks() {
                    if target.as_normal().is_some() {
                        auto_bookmark = Some(bookmark_name.as_str().to_string());
                        break;
                    }
                }
            }
        }

        match auto_bookmark {
            Some(name) => {
                // Found bookmark - collect it for push
                let ref_name: &RefName = name.as_ref();
                let target = view.get_local_bookmark(ref_name);
                if let Some(local_commit_id) = target.as_normal() {
                    let remote_symbol = ref_name.to_remote_symbol(origin_remote);
                    let remote_ref = view.get_remote_bookmark(remote_symbol);
                    let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());
                    branches_to_push.push((
                        name,
                        Some(local_commit_id.hex()),
                        remote_commit_id,
                    ));
                }
            }
            None => {
                anyhow::bail!(
                    "No bookmark to push. Run 'tl publish' first, or specify --all or -b <name>"
                );
            }
        }
    }

    if branches_to_push.is_empty() {
        anyhow::bail!("No branches to push");
    }

    // Pre-validate: Check for up-to-date branches
    let mut up_to_date_branches = Vec::new();

    for (name, local_commit, remote_commit) in &branches_to_push {
        if let (Some(local), Some(remote)) = (local_commit, remote_commit) {
            if local == remote {
                up_to_date_branches.push(name.clone());
            }
        }
    }

    // Build branch update targets (skip up-to-date branches)
    let mut branch_updates = Vec::new();
    let mut results = Vec::new();

    for (name, local_commit, remote_commit) in &branches_to_push {
        // Skip up-to-date branches
        if up_to_date_branches.contains(name) {
            results.push(BranchPushResult {
                name: name.clone(),
                status: BranchPushStatus::UpToDate,
                old_commit: remote_commit.clone(),
                new_commit: local_commit.clone(),
            });
            continue;
        }

        if let Some(local_hex) = local_commit {
            // Use hex::decode + CommitId::new to avoid lifetime issues with from_hex
            let local_commit_id = jj_lib::backend::CommitId::new(
                hex::decode(local_hex).expect("Invalid local commit hex")
            );
            let old_target = remote_commit.as_ref()
                .map(|h| jj_lib::backend::CommitId::new(
                    hex::decode(h).expect("Invalid remote commit hex")
                ));

            let ref_name = RefNameBuf::from(name.clone());
            branch_updates.push((
                ref_name,
                BookmarkPushUpdate {
                    old_target,
                    new_target: Some(local_commit_id),
                },
            ));
        }
    }

    // If nothing to push after filtering, return early
    if branch_updates.is_empty() {
        return Ok(results);
    }

    // Store names for results before moving branch_updates
    let update_names: Vec<String> = branch_updates.iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect();

    let targets = GitBranchPushTargets {
        branch_updates,
    };

    // Create remote name
    let remote_name = RemoteName::new("origin");

    // Set up callbacks (progress only - auth is handled by git subprocess)
    let callbacks = RemoteCallbacks::default();

    // Execute push using JJ's native API (uses git subprocess internally)
    match push_branches(tx.repo_mut(), &git_settings, &remote_name, &targets, callbacks) {
        Ok(stats) => {
            // Check for rejections
            if !stats.rejected.is_empty() || !stats.remote_rejected.is_empty() {
                let mut rejection_msgs = Vec::new();
                for (ref_name, reason) in &stats.rejected {
                    let reason_str = reason.as_deref().unwrap_or("unknown reason");
                    rejection_msgs.push(format!("{}: {}", ref_name.as_str(), reason_str));
                }
                for (ref_name, reason) in &stats.remote_rejected {
                    let reason_str = reason.as_deref().unwrap_or("rejected by remote");
                    rejection_msgs.push(format!("{}: {}", ref_name.as_str(), reason_str));
                }
                anyhow::bail!("Push rejected:\n{}", rejection_msgs.join("\n"));
            }

            // Push succeeded - record results
            for name in update_names {
                results.push(BranchPushResult {
                    name,
                    status: BranchPushStatus::Pushed,
                    old_commit: None,
                    new_commit: None,
                });
            }
        }
        Err(e) => {
            // Convert error and propagate
            return Err(match e {
                GitPushError::NoSuchRemote(name) => {
                    anyhow!("Remote '{}' not found. Add one with: git remote add {} <url>", name.as_str(), name.as_str())
                }
                GitPushError::RemoteName(err) => {
                    anyhow!("Invalid remote name: {}", err)
                }
                GitPushError::Subprocess(err) => {
                    let error_msg = err.to_string();
                    if error_msg.contains("authentication") || error_msg.contains("Authentication")
                        || error_msg.contains("Permission denied") || error_msg.contains("could not read Username") {
                        anyhow!(
                            "Authentication failed. Configure credentials:\n\
                             - GitHub: Use SSH keys or GitHub CLI (gh auth login && gh auth setup-git)\n\
                             - GitLab: Use SSH keys or personal access tokens\n\
                             Error: {}",
                            error_msg
                        )
                    } else if error_msg.contains("non-fast-forward") || error_msg.contains("rejected") {
                        anyhow!(
                            "Push rejected (non-fast-forward). Remote has changes you don't have.\n\
                             Try: tl pull\n\
                             Or use --force to force push (overwrites remote)"
                        )
                    } else if error_msg.contains("network") || error_msg.contains("timeout")
                        || error_msg.contains("Could not resolve") {
                        anyhow!("Network error: {}\nCheck your internet connection", error_msg)
                    } else {
                        anyhow!("Git push failed: {}", error_msg)
                    }
                }
                GitPushError::UnexpectedBackend(err) => {
                    anyhow!("Unexpected git backend error: {}", err)
                }
            });
        }
    }

    // Commit transaction
    tx.commit("push to origin")
        .context("Failed to commit push transaction")?;

    Ok(results)
}

/// Fetch from Git remote using jj-lib's native GitFetch API
///
/// This uses JJ's high-level fetch function which handles:
/// - Fetching from remote via git subprocess (handles all auth automatically)
/// - Importing new Git refs to JJ
/// - Updating remote tracking branches
///
/// # Arguments
/// * `workspace` - JJ workspace (must be git-backed)
pub fn native_git_fetch(workspace: &mut Workspace) -> Result<()> {
    // Load repo at HEAD
    let user_settings = create_user_settings()?;
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // Start transaction
    let mut tx = repo.start_transaction();

    // Get GitSettings for subprocess operations
    let git_settings = GitSettings::from_settings(&user_settings)
        .context("Failed to get git settings")?;

    // Create remote name
    let remote_name = RemoteName::new("origin");

    // Expand refspecs for fetching all branches
    // StringExpression::all() matches everything
    let refspecs = expand_fetch_refspecs(&remote_name, StringExpression::all())
        .context("Failed to expand fetch refspecs")?;

    // Set up callbacks (progress only - auth is handled by git subprocess)
    let callbacks = RemoteCallbacks::default();

    // Create GitFetch helper
    let mut git_fetch = GitFetch::new(tx.repo_mut(), &git_settings)
        .context("Failed to create git fetch helper")?;

    // Execute fetch
    git_fetch.fetch(
        &remote_name,
        refspecs,
        callbacks,
        None,  // depth
        None,  // fetch_tags_override
    ).map_err(|e| match e {
        GitFetchError::NoSuchRemote(name) => {
            anyhow!("Remote '{}' not found. Add one with: git remote add {} <url>", name.as_str(), name.as_str())
        }
        GitFetchError::RemoteName(err) => {
            anyhow!("Invalid remote name: {}", err)
        }
        GitFetchError::Subprocess(err) => {
            let error_msg = err.to_string();
            if error_msg.contains("authentication") || error_msg.contains("Authentication")
                || error_msg.contains("Permission denied") || error_msg.contains("could not read Username") {
                anyhow!(
                    "Authentication failed during fetch. Configure credentials:\n\
                     - GitHub: Use SSH keys or GitHub CLI (gh auth login && gh auth setup-git)\n\
                     - GitLab: Use SSH keys or personal access tokens\n\
                     Error: {}",
                    error_msg
                )
            } else if error_msg.contains("network") || error_msg.contains("timeout")
                || error_msg.contains("Could not resolve") {
                anyhow!("Network error: {}\nCheck your internet connection", error_msg)
            } else {
                anyhow!("Git fetch failed: {}", error_msg)
            }
        }
    })?;

    // Import fetched refs into JJ
    git_fetch.import_refs()
        .context("Failed to import fetched refs")?;

    // Commit transaction
    tx.commit("fetch from origin")
        .context("Failed to commit fetch transaction")?;

    Ok(())
}

/// Information about a remote branch
#[derive(Debug, Clone)]
pub struct RemoteBranchInfo {
    /// Branch name (e.g., "tl/main")
    pub name: String,
    /// Remote commit ID (hex)
    pub remote_commit_id: Option<String>,
    /// Local commit ID (hex) if branch exists locally
    pub local_commit_id: Option<String>,
    /// Whether local and remote have diverged
    pub is_diverged: bool,
    /// Number of commits local is ahead of remote
    pub commits_ahead: usize,
    /// Number of commits local is behind remote
    pub commits_behind: usize,
}

/// Get information about remote branches after fetch
///
/// Returns branches that have updates from remote
pub fn get_remote_branch_updates(workspace: &jj_lib::workspace::Workspace) -> Result<Vec<RemoteBranchInfo>> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();
    let mut branches = Vec::new();

    // Create RemoteName for "origin"
    let origin_remote = RemoteName::new("origin");

    // Iterate through all remote bookmarks for "origin"
    for (bookmark_name, remote_ref) in view.remote_bookmarks(&origin_remote) {
        let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());

        // Get local bookmark if exists
        let local_target = view.get_local_bookmark(bookmark_name);
        let local_commit_id = local_target.as_normal().map(|id| id.hex());

        // Determine divergence status
        let (is_diverged, commits_ahead, commits_behind) = if let (Some(local_id), Some(remote_id)) = (&local_commit_id, &remote_commit_id) {
            if local_id == remote_id {
                (false, 0, 0)
            } else {
                // For now, simplified: if different, check ancestry
                // TODO: Count actual commits ahead/behind using repo.index()
                (true, 0, 0)
            }
        } else {
            (false, 0, 0)
        };

        branches.push(RemoteBranchInfo {
            name: bookmark_name.as_str().to_string(),
            remote_commit_id,
            local_commit_id,
            is_diverged,
            commits_ahead,
            commits_behind,
        });
    }

    Ok(branches)
}

/// Information about a local branch
#[derive(Debug, Clone)]
pub struct LocalBranchInfo {
    /// Branch name (e.g., "tl/main")
    pub name: String,
    /// Commit ID (hex)
    pub commit_id: String,
    /// Whether this branch has a remote tracking branch
    pub has_remote: bool,
    /// Remote commit ID if different from local
    pub remote_commit_id: Option<String>,
}

/// Get all local branches
pub fn get_local_branches(workspace: &jj_lib::workspace::Workspace) -> Result<Vec<LocalBranchInfo>> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();
    let mut branches = Vec::new();

    // Create RemoteName for "origin"
    let origin_remote = RemoteName::new("origin");

    // Iterate through all local bookmarks
    for (bookmark_name, local_ref) in view.local_bookmarks() {
        let commit_id = match local_ref.as_normal() {
            Some(id) => id.hex(),
            None => continue, // Skip conflicted refs
        };

        // Check for remote tracking bookmark
        let remote_symbol = bookmark_name.to_remote_symbol(&origin_remote);
        let remote_ref = view.get_remote_bookmark(remote_symbol);
        let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());
        let has_remote = remote_commit_id.is_some();

        // Only include remote_commit_id if different from local
        let remote_commit_id = match &remote_commit_id {
            Some(remote_id) if remote_id != &commit_id => Some(remote_id.clone()),
            _ => None,
        };

        branches.push(LocalBranchInfo {
            name: bookmark_name.as_str().to_string(),
            commit_id,
            has_remote,
            remote_commit_id,
        });
    }

    // Sort by branch name
    branches.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(branches)
}

/// Get all remote-only branches (not present locally)
pub fn get_remote_only_branches(workspace: &jj_lib::workspace::Workspace) -> Result<Vec<RemoteBranchInfo>> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();
    let mut branches = Vec::new();

    // Create RemoteName for "origin"
    let origin_remote = RemoteName::new("origin");

    // Iterate through all remote bookmarks for "origin"
    for (bookmark_name, remote_ref) in view.remote_bookmarks(&origin_remote) {
        // Skip if there's a local bookmark
        let local_target = view.get_local_bookmark(bookmark_name);
        if local_target.is_present() {
            continue;
        }

        let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());

        branches.push(RemoteBranchInfo {
            name: bookmark_name.as_str().to_string(),
            remote_commit_id,
            local_commit_id: None,
            is_diverged: false,
            commits_ahead: 0,
            commits_behind: 0,
        });
    }

    // Sort by branch name
    branches.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(branches)
}

/// Delete a local branch
pub fn delete_local_branch(workspace: &mut jj_lib::workspace::Workspace, branch_name: &str) -> Result<()> {
    use jj_lib::op_store::RefTarget;

    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // Use branch name as-is (standard Git naming)
    let full_name = branch_name.to_string();

    // Convert to RefName for API calls
    let ref_name: &RefName = full_name.as_ref();

    // Check if branch exists
    let view = repo.view();
    if !view.get_local_bookmark(ref_name).is_present() {
        anyhow::bail!("Branch '{}' not found", full_name);
    }

    // Start transaction
    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();

    // Delete bookmark (set target to absent)
    mut_repo.set_local_bookmark_target(ref_name, RefTarget::absent());

    // Commit transaction
    tx.commit(&format!("delete branch '{}'", full_name))
        .context("Failed to commit branch deletion")?;

    Ok(())
}

/// Export a specific JJ commit (by hex ID) to a target directory
///
/// Used by pull to export remote commits to working directory
pub fn export_commit_to_dir(
    workspace: &jj_lib::workspace::Workspace,
    commit_id_hex: &str,
    target_dir: &std::path::Path,
) -> Result<()> {
    use jj_lib::backend::CommitId;

    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    // Parse commit ID from hex (use owned string to avoid lifetime issues)
    let commit_id = CommitId::new(
        hex::decode(commit_id_hex).context("Invalid commit hex")?
    );

    let commit = repo.store().get_commit(&commit_id)
        .context("Failed to get commit")?;

    // Get tree from commit (returns MergedTree directly in 0.36.0)
    let tree = commit.tree();

    // Reuse existing export function
    crate::export::export_jj_tree_to_dir(
        repo.store(),
        &tree,
        target_dir,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a git repository with remotes.
    // These tests verify module structure is correct.

    #[test]
    fn test_branch_push_status_equality() {
        assert_eq!(BranchPushStatus::Pushed, BranchPushStatus::Pushed);
        assert_eq!(BranchPushStatus::UpToDate, BranchPushStatus::UpToDate);
        assert_ne!(BranchPushStatus::Pushed, BranchPushStatus::UpToDate);
    }

    #[test]
    fn test_branch_push_result_construction() {
        let result = BranchPushResult {
            name: "tl/main".to_string(),
            status: BranchPushStatus::Pushed,
            old_commit: Some("abc123".to_string()),
            new_commit: Some("def456".to_string()),
        };

        assert_eq!(result.name, "tl/main");
        assert_eq!(result.status, BranchPushStatus::Pushed);
    }
}
