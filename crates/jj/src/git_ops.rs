//! Native Git operations using jj-lib's high-level APIs
//!
//! This module provides production-ready git push/fetch operations
//! using jj-lib's native functions that use git subprocess internally.
//! Authentication is handled by the real git binary, which properly
//! supports all credential helpers (gh auth, osxkeychain, SSH agent, etc.)

use anyhow::{anyhow, Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::config::StackedConfig;
use jj_lib::git::{
    expand_fetch_refspecs, push_branches, GitBranchPushTargets, GitFetch, GitFetchError,
    GitPushError, GitSettings, RemoteCallbacks,
};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;
use jj_lib::workspace::Workspace;
use std::sync::Arc;

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
        // Collect all Timelapse bookmarks (`tl/*`) only.
        for (bookmark_name, target) in view.local_bookmarks() {
            if !bookmark_name.as_str().starts_with("tl/") {
                continue;
            }
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
        // Auto-detect: prefer Timelapse bookmarks, then fall back.
        let mut auto_bookmark: Option<String> = None;

        // Priority 1: tl/main
        let tl_main_ref: &RefName = "tl/main".as_ref();
        if view.get_local_bookmark(tl_main_ref).is_present() {
            auto_bookmark = Some("tl/main".to_string());
        } else {
            // Priority 2: tl/master
            let tl_master_ref: &RefName = "tl/master".as_ref();
            if view.get_local_bookmark(tl_master_ref).is_present() {
                auto_bookmark = Some("tl/master".to_string());
            } else {
                // Priority 3: First available tl/* bookmark
                for (bookmark_name, target) in view.local_bookmarks() {
                    if !bookmark_name.as_str().starts_with("tl/") {
                        continue;
                    }
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
    native_git_fetch_filtered(workspace, StringExpression::all())
}

/// Fetch from Git remote using jj-lib's native GitFetch API, but only for
/// bookmarks matched by `bookmark_expr`.
///
/// This is useful for latency-sensitive operations (like `tl pull`) where
/// fetching all branches on a large remote can dominate runtime.
pub fn native_git_fetch_filtered(workspace: &mut Workspace, bookmark_expr: StringExpression) -> Result<()> {
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

    // Expand refspecs for fetching selected branches (refs/heads/* patterns)
    let refspecs = expand_fetch_refspecs(&remote_name, bookmark_expr)
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

/// Latency-optimized fetch for `tl pull`.
///
/// Fetches a small set of branches:
/// - `main`, `master`
/// - every local bookmark name (so tracking branches stay up to date)
///
/// This avoids fetching every branch from large remotes by default.
pub fn native_git_fetch_for_pull(workspace: &mut Workspace) -> Result<()> {
    use std::collections::HashSet;

    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();

    let mut names: HashSet<String> = HashSet::new();
    names.insert("main".to_string());
    names.insert("master".to_string());

    for (bookmark_name, target) in view.local_bookmarks() {
        if target.as_normal().is_some() {
            names.insert(bookmark_name.as_str().to_string());
        }
    }

    let mut exprs = Vec::with_capacity(names.len());
    for name in names {
        exprs.push(StringExpression::exact(name));
    }

    native_git_fetch_filtered(workspace, StringExpression::union_all(exprs))
}

/// Get information about remote branches after fetch without expensive graph
/// traversal (ahead/behind counts are set to 0).
///
/// Intended for fast-path UX (e.g. `tl pull`, `tl fetch`) where we only need
/// to know which branches changed and whether they're diverged.
pub fn get_remote_branch_updates_fast(workspace: &jj_lib::workspace::Workspace) -> Result<Vec<RemoteBranchInfo>> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();
    let mut branches = Vec::new();

    let origin_remote = RemoteName::new("origin");
    let index = repo.index();

    // Iterate through all remote bookmarks for "origin"
    for (bookmark_name, remote_ref) in view.remote_bookmarks(&origin_remote) {
        let remote_commit_id = remote_ref.target.as_normal().map(|id| id.hex());

        // Get local bookmark if exists
        let local_target = view.get_local_bookmark(bookmark_name);
        let local_commit_id = local_target.as_normal().map(|id| id.hex());

        // Diverged if neither is ancestor of the other (cheap via index).
        let is_diverged = if let (Some(local_target), Some(remote_target)) =
            (local_target.as_normal(), remote_ref.target.as_normal())
        {
            if local_target.hex() == remote_target.hex() {
                false
            } else {
                let local_ancestor = index.is_ancestor(local_target, remote_target)
                    .context("Failed to check ancestry (local->remote)")?;
                let remote_ancestor = index.is_ancestor(remote_target, local_target)
                    .context("Failed to check ancestry (remote->local)")?;
                !local_ancestor && !remote_ancestor
            }
        } else {
            false
        };

        branches.push(RemoteBranchInfo {
            name: bookmark_name.as_str().to_string(),
            remote_commit_id,
            local_commit_id,
            is_diverged,
            commits_ahead: 0,
            commits_behind: 0,
        });
    }

    // Sort by branch name
    branches.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(branches)
}

/// Return the first matching remote branch head (name + commit hex) for origin.
///
/// This avoids building a full branch list and avoids expensive ahead/behind
/// computations. Used by `tl pull` to pick a sync target quickly.
pub fn get_preferred_remote_head(
    workspace: &jj_lib::workspace::Workspace,
    preferred: &[&str],
) -> Result<Option<(String, String)>> {
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repository")?;

    let view = repo.view();
    let origin_remote = RemoteName::new("origin");

    for name in preferred {
        let ref_name: &RefName = (*name).as_ref();
        let remote_symbol = ref_name.to_remote_symbol(&origin_remote);
        let remote_ref = view.get_remote_bookmark(remote_symbol);
        if let Some(id) = remote_ref.target.as_normal() {
            return Ok(Some((name.to_string(), id.hex())));
        }
    }

    for (bookmark_name, remote_ref) in view.remote_bookmarks(&origin_remote) {
        if let Some(id) = remote_ref.target.as_normal() {
            return Ok(Some((bookmark_name.as_str().to_string(), id.hex())));
        }
    }

    Ok(None)
}

/// Calculate how many commits ahead and behind two branches are
///
/// Returns (commits_ahead, commits_behind) where:
/// - commits_ahead: number of commits in local_id not reachable from remote_id
/// - commits_behind: number of commits in remote_id not reachable from local_id
///
/// Uses commit graph traversal to count the exact number of commits.
fn calculate_ahead_behind(
    repo: &Arc<ReadonlyRepo>,
    local_id: &CommitId,
    remote_id: &CommitId,
) -> Result<(usize, usize)> {
    use std::collections::HashSet;

    // If commits are the same, no difference
    if local_id == remote_id {
        return Ok((0, 0));
    }

    // Find common ancestor by walking both histories
    // We use BFS from both commits and find the intersection
    let mut local_ancestors: HashSet<CommitId> = HashSet::new();
    let mut remote_ancestors: HashSet<CommitId> = HashSet::new();

    // Walk local history
    let mut local_queue = vec![local_id.clone()];
    while let Some(commit_id) = local_queue.pop() {
        if local_ancestors.contains(&commit_id) {
            continue;
        }
        local_ancestors.insert(commit_id.clone());

        // Get parent commits
        if let Ok(commit) = repo.store().get_commit(&commit_id) {
            for parent_id in commit.parent_ids() {
                if !local_ancestors.contains(parent_id) {
                    local_queue.push(parent_id.clone());
                }
            }
        }
    }

    // Walk remote history
    let mut remote_queue = vec![remote_id.clone()];
    while let Some(commit_id) = remote_queue.pop() {
        if remote_ancestors.contains(&commit_id) {
            continue;
        }
        remote_ancestors.insert(commit_id.clone());

        // Get parent commits
        if let Ok(commit) = repo.store().get_commit(&commit_id) {
            for parent_id in commit.parent_ids() {
                if !remote_ancestors.contains(parent_id) {
                    remote_queue.push(parent_id.clone());
                }
            }
        }
    }

    // Find common ancestors (intersection)
    let common: HashSet<_> = local_ancestors.intersection(&remote_ancestors).cloned().collect();

    // Count commits ahead: local commits not in remote ancestors (excluding common)
    let commits_ahead = local_ancestors
        .iter()
        .filter(|id| !remote_ancestors.contains(*id))
        .count();

    // Count commits behind: remote commits not in local ancestors (excluding common)
    let commits_behind = remote_ancestors
        .iter()
        .filter(|id| !local_ancestors.contains(*id))
        .count();

    Ok((commits_ahead, commits_behind))
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
        let (is_diverged, commits_ahead, commits_behind) = if let (Some(local_target), Some(remote_target)) = (local_target.as_normal(), remote_ref.target.as_normal()) {
            if local_target.hex() == remote_target.hex() {
                (false, 0, 0)
            } else {
                // Calculate actual commits ahead/behind using commit graph
                match calculate_ahead_behind(&repo, local_target, remote_target) {
                    Ok((ahead, behind)) => {
                        let is_diverged = ahead > 0 && behind > 0;
                        (is_diverged, ahead, behind)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to calculate ahead/behind for {}: {}", bookmark_name.as_str(), e);
                        // Fallback: branches are different but we don't know the count
                        (true, 0, 0)
                    }
                }
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

/// Export a specific JJ commit (by hex ID) directly into the Timelapse store.
///
/// Returns `(root_tree_hash, entries_written)`.
pub fn export_commit_to_tl_store(
    workspace: &jj_lib::workspace::Workspace,
    commit_id_hex: &str,
    tl_store: &tl_core::Store,
) -> Result<(tl_core::Sha1Hash, u32)> {
    use jj_lib::backend::CommitId;

    let repo = workspace
        .repo_loader()
        .load_at_head()
        .context("Failed to load repository")?;

    let commit_id = CommitId::new(hex::decode(commit_id_hex).context("Invalid commit hex")?);
    let commit = repo
        .store()
        .get_commit(&commit_id)
        .context("Failed to get commit")?;

    let tree = commit.tree();
    crate::export::export_jj_tree_to_tl_store(repo.store(), &tree, tl_store)
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
