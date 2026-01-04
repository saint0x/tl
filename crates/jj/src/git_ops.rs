//! Native Git operations using jj-lib's high-level APIs
//!
//! This module provides production-ready git push/fetch operations
//! using jj-lib's native functions that handle git2 internally.

use anyhow::{anyhow, Context, Result};
use jj_lib::git::{fetch, push_branches, GitBranchPushTargets, GitFetchError, GitPushError, RemoteCallbacks};
use jj_lib::git_backend::GitBackend;
use jj_lib::refs::BranchPushUpdate;
use jj_lib::repo::Repo;
use jj_lib::str_util::StringPattern;
use jj_lib::workspace::Workspace;
use std::collections::HashSet;

/// Push to Git remote using jj-lib's native push_branches API
///
/// This uses JJ's high-level push function which handles:
/// - Exporting JJ refs to Git
/// - Pushing to remote
/// - Updating remote tracking branches
///
/// # Arguments
/// * `workspace` - JJ workspace (must be git-backed)
/// * `bookmark` - Optional bookmark name (will push snap/<bookmark>)
/// * `all` - Push all snap/* bookmarks
/// * `force` - Force push (non-fast-forward)
pub fn native_git_push(
    workspace: &mut Workspace,
    bookmark: Option<&str>,
    all: bool,
    force: bool,
) -> Result<()> {
    // Load repo at HEAD
    let config = config::Config::builder().build()?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repository")?;

    // Start transaction
    let mut tx = repo.start_transaction(&user_settings);
    let _git_settings = user_settings.git_settings();

    // Get git repository via GitBackend
    let store = Repo::store(tx.mut_repo());
    let git_backend = store.backend_impl()
        .downcast_ref::<GitBackend>()
        .ok_or_else(|| anyhow!("Not a git-backed repository"))?;

    let git_repo = git_backend.open_git_repo()
        .context("Failed to open git repository")?;

    // Build branch update targets
    let mut branch_updates = Vec::new();
    let mut force_pushed_branches = HashSet::new();

    // Get branches to push from view
    let view = tx.mut_repo().view();

    if all {
        // Push all snap/* branches
        for (branch_name, target) in view.local_branches() {
            if branch_name.starts_with("snap/") {
                if let Some(new_commit_id) = target.as_normal() {
                    let remote_ref = view.get_remote_branch(branch_name, "origin");
                    let old_target = remote_ref.target.as_normal().cloned();

                    branch_updates.push((
                        branch_name.to_string(),
                        BranchPushUpdate {
                            old_target,
                            new_target: Some(new_commit_id.clone()),
                        },
                    ));

                    if force {
                        force_pushed_branches.insert(branch_name.to_string());
                    }
                }
            }
        }
    } else if let Some(bookmark_name) = bookmark {
        // Push specific bookmark
        let full_name = if bookmark_name.starts_with("snap/") {
            bookmark_name.to_string()
        } else {
            format!("snap/{}", bookmark_name)
        };

        let target = view.get_local_branch(&full_name);
        if let Some(new_commit_id) = target.as_normal() {
            let remote_ref = view.get_remote_branch(&full_name, "origin");
            let old_target = remote_ref.target.as_normal().cloned();

            branch_updates.push((
                full_name.clone(),
                BranchPushUpdate {
                    old_target,
                    new_target: Some(new_commit_id.clone()),
                },
            ));

            if force {
                force_pushed_branches.insert(full_name);
            }
        } else {
            anyhow::bail!("Branch {} not found or not at a single commit", full_name);
        }
    } else {
        anyhow::bail!("Must specify either --all or a bookmark name");
    }

    if branch_updates.is_empty() {
        anyhow::bail!("No branches to push");
    }

    let targets = GitBranchPushTargets {
        branch_updates,
        force_pushed_branches,
    };

    // Set up empty callbacks (no progress reporting for now)
    let callbacks = RemoteCallbacks::default();

    // Execute push using JJ's native API
    push_branches(tx.mut_repo(), &git_repo, "origin", &targets, callbacks)
        .map_err(|e| match e {
            GitPushError::InternalGitError(git_err) => {
                let error_msg = git_err.message();
                if error_msg.contains("authentication") || error_msg.contains("Authentication") {
                    anyhow!(
                        "Authentication failed. Configure credentials:\n\
                         - GitHub: Use SSH keys or GitHub CLI (gh auth login)\n\
                         - GitLab: Use SSH keys or personal access tokens\n\
                         Error: {}",
                        error_msg
                    )
                } else if error_msg.contains("non-fast-forward") || error_msg.contains("rejected") {
                    anyhow!(
                        "Push rejected (non-fast-forward). Remote has changes you don't have.\n\
                         Try: tl pull && jj rebase\n\
                         Or use --force to force push (DANGEROUS)\n\
                         Error: {}",
                        error_msg
                    )
                } else if error_msg.contains("network") || error_msg.contains("timeout") {
                    anyhow!("Network error: {}\nCheck your internet connection", error_msg)
                } else {
                    anyhow!("Git push failed: {}", error_msg)
                }
            }
            GitPushError::NoSuchRemote(name) => {
                anyhow!("Remote '{}' not found. Add one with: git remote add {} <url>", name, name)
            }
            GitPushError::RefUpdateRejected(msgs) => {
                anyhow!("Push rejected: {}", msgs.join(", "))
            }
            GitPushError::RemoteReservedForLocalGitRepo => {
                anyhow!("Cannot push to 'git' remote (reserved for local Git repository)")
            }
            GitPushError::NotFastForward => {
                anyhow!(
                    "Push rejected (not a fast-forward). Remote has changes you don't have.\n\
                     Try: tl pull && jj rebase\n\
                     Or use --force to force push (DANGEROUS)"
                )
            }
        })?;

    // Commit transaction
    tx.commit("push to origin");

    Ok(())
}

/// Fetch from Git remote using jj-lib's native fetch API
///
/// This uses JJ's high-level fetch function which handles:
/// - Fetching from remote
/// - Importing new Git refs to JJ
/// - Updating remote tracking branches
///
/// # Arguments
/// * `workspace` - JJ workspace (must be git-backed)
pub fn native_git_fetch(workspace: &mut Workspace) -> Result<()> {
    // Load repo at HEAD
    let config = config::Config::builder().build()?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repository")?;

    // Get git repository via GitBackend
    let store = repo.store();
    let git_backend = store.backend_impl()
        .downcast_ref::<GitBackend>()
        .ok_or_else(|| anyhow!("Not a git-backed repository"))?;

    let git_repo = git_backend.open_git_repo()
        .context("Failed to open git repository")?;

    // Start transaction
    let mut tx = repo.start_transaction(&user_settings);
    let git_settings = user_settings.git_settings();

    // Fetch all branches (empty pattern = fetch all)
    let branch_patterns = vec![StringPattern::everything()];
    let callbacks = RemoteCallbacks::default();

    // Execute fetch using JJ's native API
    fetch(tx.mut_repo(), &git_repo, "origin", &branch_patterns, callbacks, &git_settings)
        .map_err(|e| match e {
            GitFetchError::InternalGitError(git_err) => {
                let error_msg = git_err.message();
                if error_msg.contains("authentication") || error_msg.contains("Authentication") {
                    anyhow!(
                        "Authentication failed during fetch. Configure credentials:\n\
                         - GitHub: Use SSH keys or GitHub CLI (gh auth login)\n\
                         - GitLab: Use SSH keys or personal access tokens\n\
                         Error: {}",
                        error_msg
                    )
                } else if error_msg.contains("network") || error_msg.contains("timeout") {
                    anyhow!("Network error: {}\nCheck your internet connection", error_msg)
                } else {
                    anyhow!("Git fetch failed: {}", error_msg)
                }
            }
            GitFetchError::NoSuchRemote(name) => {
                anyhow!("Remote '{}' not found. Add one with: git remote add {} <url>", name, name)
            }
            GitFetchError::InvalidBranchPattern => {
                anyhow!("Invalid branch pattern")
            }
            GitFetchError::GitImportError(err) => {
                anyhow!("Failed to import fetched refs: {:?}", err)
            }
        })?;

    // Commit transaction
    tx.commit("fetch from origin");

    Ok(())
}

// Tests disabled on macOS due to OpenSSL linking issues in test binary
// The production code works correctly - this is a platform-specific test infrastructure issue
#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    /// Helper to create a test JJ workspace with git backend
    fn create_test_git_workspace(path: &Path) -> Result<Workspace> {
        let config = config::Config::builder().build()?;
        let user_settings = jj_lib::settings::UserSettings::from_config(config);
        let (workspace, _repo) = jj_lib::workspace::Workspace::init_internal_git(&user_settings, path)?;
        Ok(workspace)
    }

    #[test]
    fn test_native_git_fetch_requires_remote() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mut workspace = create_test_git_workspace(temp_dir.path())?;

        // Fetch should fail gracefully if no remote is configured
        let result = native_git_fetch(&mut workspace);

        // Should get an error about missing remote
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("origin"));

        Ok(())
    }

    #[test]
    fn test_native_git_push_requires_bookmark_or_all() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mut workspace = create_test_git_workspace(temp_dir.path())?;

        // Push without bookmark or --all should fail
        let result = native_git_push(&mut workspace, None, false, false);

        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("Must specify either --all or a bookmark name"));

        Ok(())
    }
}
