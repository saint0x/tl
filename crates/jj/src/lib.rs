//! JJ (Jujutsu) integration for Git interoperability
//!
//! This crate provides:
//! - Checkpoint → JJ commit materialization
//! - `tl publish` (create JJ commit from checkpoint)
//! - `tl push` / `tl pull` (Git interop via JJ)
//! - Checkpoint ↔ JJ commit mapping
//!
//! All operations are designed to be configurable via CLI flags to give users
//! maximum control over behavior.

pub mod export;
pub mod git_ops;
pub mod mapping;
pub mod materialize;
pub mod publish;
pub mod workspace;

// Re-export public types
pub use mapping::JjMapping;
pub use materialize::{CommitMessageOptions, PublishOptions};
pub use publish::{publish_checkpoint, publish_range};
pub use workspace::{validate_workspace_name, JjWorkspace, WorkspaceManager, WorkspaceState};

use anyhow::{Context, Result};
use jj_lib::backend::ObjectId;    // For CommitId parsing
use jj_lib::repo::Repo;            // For Repo trait methods
use std::path::{Path, PathBuf};

/// Errors specific to JJ integration
#[derive(Debug, thiserror::Error)]
pub enum JjError {
    #[error("JJ workspace not found. Run 'jj git init' first.")]
    WorkspaceNotFound,

    #[error("JJ workspace invalid: {0}")]
    InvalidWorkspace(String),

    #[error("Failed to create JJ commit: {0}")]
    CommitFailed(String),

    #[error("Failed to import JJ tree: {0}")]
    ImportFailed(String),

    #[error("Checkpoint not mapped to JJ commit")]
    NoMapping,

    #[error("JJ operation failed: {0}")]
    OperationFailed(String),
}

/// Detect if a JJ workspace exists at the repository root
///
/// Returns Some(PathBuf) if .jj/ directory exists and appears valid,
/// None otherwise.
pub fn detect_jj_workspace(repo_root: &Path) -> Result<Option<PathBuf>> {
    let jj_dir = repo_root.join(".jj");

    if !jj_dir.exists() || !jj_dir.is_dir() {
        return Ok(None);
    }

    // Validate it's a proper JJ workspace by checking for required subdirectories
    // JJ stores data in .jj/repo/ or .jj/store/ depending on version
    let has_repo = jj_dir.join("repo").exists();
    let has_store = jj_dir.join("store").exists();

    if has_repo || has_store {
        Ok(Some(jj_dir))
    } else {
        Ok(None)
    }
}

/// Load a JJ workspace from the repository root
///
/// This initializes the JJ workspace using jj-lib's APIs.
///
/// # Errors
///
/// Returns `JjError::WorkspaceNotFound` if no .jj/ directory exists.
/// Returns `JjError::InvalidWorkspace` if the workspace cannot be loaded.
pub fn load_workspace(repo_root: &Path) -> Result<jj_lib::workspace::Workspace> {
    use jj_lib::repo::StoreFactories;
    use jj_lib::local_working_copy::LocalWorkingCopy;
    use std::collections::HashMap;
    use std::sync::Arc;

    // First check if workspace exists
    detect_jj_workspace(repo_root)?
        .ok_or(JjError::WorkspaceNotFound)?;

    // Create default user settings from empty config
    let config = config::Config::builder().build()
        .map_err(|e| JjError::InvalidWorkspace(format!("Failed to create config: {}", e)))?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);

    // Create default store factories
    let store_factories = StoreFactories::default();

    // Register the local working copy factory (required for production)
    let mut working_copy_factories = HashMap::new();
    working_copy_factories.insert(
        "local".to_string(),
        Box::new(|store: &Arc<jj_lib::store::Store>, working_copy_path: &std::path::Path, state_path: &std::path::Path| {
            Box::new(LocalWorkingCopy::load(
                store.clone(),
                working_copy_path.to_path_buf(),
                state_path.to_path_buf(),
            )) as Box<dyn jj_lib::working_copy::WorkingCopy>
        }) as jj_lib::workspace::WorkingCopyFactory,
    );

    // Load the workspace
    let workspace = jj_lib::workspace::Workspace::load(
        &user_settings,
        repo_root,
        &store_factories,
        &working_copy_factories,
    )
    .map_err(|e| JjError::InvalidWorkspace(e.to_string()))?;

    Ok(workspace)
}

/// Initialize JJ with colocated git (creates both .git and .jj)
///
/// This function creates a new JJ workspace with a colocated Git repository,
/// where .git/ lives in the repository root alongside .jj/. This is the
/// recommended setup for new projects.
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if initialization fails.
pub fn init_jj_colocated(repo_root: &Path) -> Result<()> {
    use jj_lib::workspace::Workspace;

    // Create default user settings
    let config = config::Config::builder().build()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create config: {}", e)))?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);

    // Initialize colocated workspace
    Workspace::init_colocated_git(&user_settings, repo_root)
        .map_err(|e| JjError::OperationFailed(format!("Failed to initialize JJ workspace: {}", e)))?;

    Ok(())
}

/// Initialize JJ with existing git (creates .jj only, links to .git)
///
/// This function creates a JJ workspace that links to an existing Git repository.
/// The Git repository at `git_dir` is used as the backend store.
///
/// # Arguments
///
/// * `repo_root` - The repository root where .jj/ will be created
/// * `git_dir` - Path to the existing .git directory (usually repo_root/.git)
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if initialization fails.
pub fn init_jj_external(repo_root: &Path, git_dir: &Path) -> Result<()> {
    use jj_lib::workspace::Workspace;

    // Create default user settings
    let config = config::Config::builder().build()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create config: {}", e)))?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);

    // Initialize workspace with external git backend
    Workspace::init_external_git(&user_settings, repo_root, git_dir)
        .map_err(|e| JjError::OperationFailed(format!("Failed to initialize JJ with external git: {}", e)))?;

    Ok(())
}

/// Configure JJ bookmarks for optimal timelapse workflow
///
/// Sets up JJ configuration for:
/// - Bookmark prefix for timelapse snapshots (snap/)
/// - Default revset for log display
/// - Empty default commit description
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if configuration fails.
/// Warnings are logged but don't cause failure.
pub fn configure_jj_bookmarks(repo_root: &Path) -> Result<()> {
    // Configure JJ settings via jj config command
    // These settings make the timelapse workflow smoother

    let configs = vec![
        ("revsets.log", "bookmarks() | @"),
        ("git.push-bookmark-prefix", "snap/"),
        ("ui.default-description", ""),
    ];

    for (key, value) in configs {
        let status = std::process::Command::new("jj")
            .args(&["config", "set", "--repo", key, value])
            .current_dir(repo_root)
            .status();

        match status {
            Ok(s) if s.success() => {
                // Successfully set config
            }
            Ok(_) | Err(_) => {
                // Failed to set config - this is optional, so we continue
            }
        }
    }

    Ok(())
}

/// Check if JJ binary is available in PATH
///
/// This is useful for operations that shell out to JJ CLI (like git fetch/push).
pub fn check_jj_binary() -> Result<bool> {
    let output = std::process::Command::new("which")
        .arg("jj")
        .output();

    match output {
        Ok(output) => Ok(output.status.success()),
        Err(_) => Ok(false),
    }
}

/// Create a JJ bookmark (branch) using native jj-lib APIs
///
/// Creates a bookmark pointing to the specified commit. The bookmark name
/// is automatically prefixed with "snap/" if not already prefixed.
///
/// # Arguments
///
/// * `workspace` - JJ workspace (must be loaded)
/// * `bookmark_name` - Name of the bookmark to create (e.g., "feature" or "snap/feature")
/// * `commit_id` - Hex string of the commit ID to point the bookmark at
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if bookmark creation fails.
pub fn create_bookmark_native(
    workspace: &mut jj_lib::workspace::Workspace,
    bookmark_name: &str,
    commit_id: &str,
) -> Result<()> {
    use jj_lib::backend::CommitId;
    use jj_lib::op_store::RefTarget;

    // Load user settings and repo
    let config = config::Config::builder().build()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create config: {}", e)))?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);

    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repo")?;

    // Parse commit ID using ObjectId trait
    let commit_id_bytes = hex::decode(commit_id)
        .context("Invalid commit ID hex string")?;
    let commit_id = CommitId::from_bytes(&commit_id_bytes);

    // Ensure snap/ prefix
    let full_name = if bookmark_name.starts_with("snap/") {
        bookmark_name.to_string()
    } else {
        format!("snap/{}", bookmark_name)
    };

    // Start transaction
    let mut tx = repo.start_transaction(&user_settings);
    let mut_repo = tx.mut_repo();

    // Check if bookmark exists (using Repo trait)
    let view = Repo::view(mut_repo);
    if view.get_local_branch(&full_name).is_present() {
        anyhow::bail!("Bookmark '{}' already exists", full_name);
    }

    // Set bookmark target (using MutableRepo method)
    mut_repo.set_local_branch_target(&full_name, RefTarget::normal(commit_id));

    // Commit transaction (returns Arc<ReadonlyRepo>, no Result to handle)
    let _committed_repo = tx.commit("Create bookmark");

    Ok(())
}

/// Configure JJ repository settings using native config APIs
///
/// Sets up JJ configuration for:
/// - Bookmark prefix for timelapse snapshots (snap/)
/// - Default revset for log display
/// - Empty default commit description
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if configuration fails.
pub fn configure_jj_native(repo_root: &Path) -> Result<()> {
    use std::fs;

    // JJ stores repo-level config in .jj/repo/config.toml
    let config_path = repo_root.join(".jj/repo/config.toml");

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .context("Failed to create .jj/repo directory")?;
    }

    // Create TOML config content
    let config_content = r#"# Timelapse JJ Configuration

[revsets]
log = "bookmarks() | @"

[git]
push-bookmark-prefix = "snap/"

[ui]
default-description = ""
"#;

    // Write config file (overwrites if exists, but that's fine - we want these settings)
    fs::write(&config_path, config_content)
        .context("Failed to write JJ config")?;

    Ok(())
}

/// Add a JJ workspace using native jj-lib APIs
///
/// Creates a new workspace in the JJ repository. Workspaces allow multiple
/// working copies of the same repository with independent working states.
///
/// # Arguments
///
/// * `repo_root` - The main repository root
/// * `workspace_name` - Name of the new workspace
/// * `workspace_path` - Path where the workspace should be created
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if workspace creation fails.
pub fn add_workspace_native(
    repo_root: &Path,
    workspace_name: &str,
    workspace_path: &Path,
) -> Result<()> {
    use jj_lib::workspace::Workspace;

    // Load main workspace and repo
    let workspace = load_workspace(repo_root)?;

    let config = config::Config::builder().build()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create config: {}", e)))?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repo")?;

    // Use default working copy initializer
    let working_copy_initializer = jj_lib::workspace::default_working_copy_initializer();

    // Convert workspace name to WorkspaceId
    let workspace_id = jj_lib::op_store::WorkspaceId::new(workspace_name.to_string());

    // Call the official API with correct parameter order:
    // init_workspace_with_existing_repo(user_settings, workspace_root, repo, initializer, workspace_id)
    let (_new_workspace, _new_repo) = Workspace::init_workspace_with_existing_repo(
        &user_settings,
        workspace_path,
        &repo,
        working_copy_initializer,
        workspace_id,
    ).map_err(|e| JjError::OperationFailed(format!("Failed to add workspace: {}", e)))?;

    Ok(())
}

/// Forget (remove) a JJ workspace using native jj-lib APIs
///
/// Removes a workspace from JJ tracking. This does not delete the workspace
/// directory or its files - it only removes JJ's awareness of the workspace.
///
/// # Arguments
///
/// * `repo_root` - The main repository root
/// * `workspace_name` - Name of the workspace to forget
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if workspace removal fails.
pub fn forget_workspace_native(
    repo_root: &Path,
    workspace_name: &str,
) -> Result<()> {
    // Load workspace and repo
    let workspace = load_workspace(repo_root)?;

    let config = config::Config::builder().build()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create config: {}", e)))?;
    let user_settings = jj_lib::settings::UserSettings::from_config(config);
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repo")?;

    // Convert to WorkspaceId
    let ws_id = jj_lib::op_store::WorkspaceId::new(workspace_name.to_string());

    // Check if workspace exists by checking for its wc commit
    let view = Repo::view(repo.as_ref());
    if view.get_wc_commit_id(&ws_id).is_none() {
        anyhow::bail!("Workspace '{}' not found", workspace_name);
    }

    // Start transaction to remove workspace
    let mut tx = repo.start_transaction(&user_settings);
    tx.mut_repo().remove_wc_commit(&ws_id);

    // Commit transaction (returns Arc<ReadonlyRepo>, no Result to handle)
    let description = format!("forget workspace '{}'", workspace_name);
    let _committed_repo = tx.commit(&description);

    Ok(())
}
