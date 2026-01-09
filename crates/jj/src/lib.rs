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

pub mod conflicts;
pub mod export;
pub mod git_ops;
pub mod mapping;
pub mod materialize;
pub mod merge;
pub mod publish;
pub mod workspace;

// Re-export public types
pub use conflicts::{
    has_conflict_markers, write_conflict_markers, count_conflicts,
    parse_conflict_regions, is_resolved, check_resolution_status,
    ConflictRegion, ResolutionStatus,
    CONFLICT_MARKER_START, CONFLICT_MARKER_END,
};
pub use git_ops::{RemoteBranchInfo, BranchPushResult, BranchPushStatus, LocalBranchInfo};
pub use mapping::{JjMapping, SEED_COMMIT_KEY};
pub use materialize::{CommitMessageOptions, PublishOptions};
pub use merge::{
    MergeResult, MergeState, ConflictInfo,
    perform_merge, find_merge_base, get_branch_commit_id, get_current_commit_id,
};
pub use publish::{publish_checkpoint, publish_range};
pub use workspace::{validate_workspace_name, JjWorkspace, WorkspaceManager, WorkspaceState};

use anyhow::{anyhow, Context, Result};
use jj_lib::config::StackedConfig;
use jj_lib::object_id::ObjectId;    // For CommitId parsing
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo;            // For Repo trait methods
use jj_lib::settings::UserSettings;
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

/// Create default UserSettings for jj-lib operations
pub fn create_user_settings() -> Result<UserSettings> {
    let config = StackedConfig::with_defaults();
    UserSettings::from_config(config)
        .map_err(|e| anyhow!("Failed to create user settings: {}", e))
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

    // First check if workspace exists
    detect_jj_workspace(repo_root)?
        .ok_or(JjError::WorkspaceNotFound)?;

    // Create default user settings
    let user_settings = create_user_settings()
        .map_err(|e| JjError::InvalidWorkspace(format!("Failed to create user settings: {}", e)))?;

    // Create default store factories
    let store_factories = StoreFactories::default();

    // Use default working copy factories
    let working_copy_factories = jj_lib::workspace::default_working_copy_factories();

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
    let user_settings = create_user_settings()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create user settings: {}", e)))?;

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
    let user_settings = create_user_settings()
        .map_err(|e| JjError::OperationFailed(format!("Failed to create user settings: {}", e)))?;

    // Initialize workspace with external git backend
    Workspace::init_external_git(&user_settings, repo_root, git_dir)
        .map_err(|e| JjError::OperationFailed(format!("Failed to initialize JJ with external git: {}", e)))?;

    Ok(())
}

/// Configure JJ bookmarks for optimal timelapse workflow
///
/// Sets up JJ configuration for:
/// - Bookmark prefix for timelapse snapshots (tl/)
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
        ("git.push-bookmark-prefix", ""),
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
/// Creates a bookmark pointing to the specified commit using standard Git branch naming.
///
/// # Arguments
///
/// * `workspace` - JJ workspace (must be loaded)
/// * `bookmark_name` - Name of the bookmark to create (e.g., "main", "feature", "fix/bug-123")
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

    // Load repo
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repo")?;

    // Parse commit ID using ObjectId trait
    let commit_id_bytes = hex::decode(commit_id)
        .context("Invalid commit ID hex string")?;
    let commit_id = CommitId::from_bytes(&commit_id_bytes);

    // Use bookmark name directly (standard Git naming)
    let full_name = bookmark_name.to_string();

    // Start transaction
    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();

    // Get ref name for bookmark operations
    let ref_name: &jj_lib::ref_name::RefName = full_name.as_ref();

    // Check if bookmark exists - if so, we'll update it
    let view = mut_repo.view();
    let bookmark_exists = view.get_local_bookmark(ref_name).is_present();

    // Set bookmark target (creates if new, updates if exists)
    mut_repo.set_local_bookmark_target(ref_name, RefTarget::normal(commit_id));

    // Commit transaction with appropriate message
    let commit_msg = if bookmark_exists {
        "Update bookmark"
    } else {
        "Create bookmark"
    };
    tx.commit(commit_msg)
        .context("Failed to commit bookmark operation")?;

    Ok(())
}

/// Configure JJ repository settings using native config APIs
///
/// Sets up JJ configuration for:
/// - Bookmark prefix for timelapse snapshots (tl/)
/// - Default revset for log display
/// - Empty default commit description
///
/// # Errors
///
/// Returns `JjError::OperationFailed` if configuration fails.
pub fn configure_jj_native(repo_root: &Path) -> Result<()> {
    configure_jj_with_user(repo_root, None, None)
}

/// Configure JJ workspace with optional user identity
///
/// Sets up JJ repo-level configuration including:
/// - Bookmark prefix for timelapse workflow
/// - User name and email (required for pushing commits)
///
/// # Arguments
///
/// * `repo_root` - Path to the repository root
/// * `user_name` - Optional user name (from git config)
/// * `user_email` - Optional user email (from git config)
pub fn configure_jj_with_user(
    repo_root: &Path,
    user_name: Option<&str>,
    user_email: Option<&str>,
) -> Result<()> {
    use std::fs;

    // JJ stores repo-level config in .jj/repo/config.toml
    let config_path = repo_root.join(".jj/repo/config.toml");

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .context("Failed to create .jj/repo directory")?;
    }

    // Build config content with user section if provided
    let user_section = match (user_name, user_email) {
        (Some(name), Some(email)) => format!(
            r#"
[user]
name = "{}"
email = "{}"
"#,
            name, email
        ),
        _ => String::new(),
    };

    // Create TOML config content
    let config_content = format!(
        r#"# Timelapse JJ Configuration
{}
[revsets]
log = "bookmarks() | @"

[git]
push-bookmark-prefix = ""

[ui]
default-description = ""
"#,
        user_section
    );

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
    use jj_lib::ref_name::WorkspaceNameBuf;
    use jj_lib::workspace::Workspace;

    // Load main workspace and repo
    let workspace = load_workspace(repo_root)?;
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repo")?;

    // Get the repo path (usually .jj/repo)
    let repo_path = repo_root.join(".jj/repo");

    // Use default working copy factory (not factories hashmap)
    let working_copy_factory = jj_lib::workspace::default_working_copy_factory();

    // Convert workspace name to WorkspaceNameBuf
    let workspace_name_buf = WorkspaceNameBuf::from(workspace_name.to_string());

    // Call the official API (0.36.0 signature has repo_path as 2nd arg)
    let (_new_workspace, _new_repo) = Workspace::init_workspace_with_existing_repo(
        workspace_path,
        &repo_path,
        &repo,
        &*working_copy_factory,
        workspace_name_buf,
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
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repo")?;

    // Convert to WorkspaceName
    let ws_name = WorkspaceName::new(workspace_name);

    // Check if workspace exists by checking for its wc commit
    let view = Repo::view(repo.as_ref());
    if view.get_wc_commit_id(ws_name).is_none() {
        anyhow::bail!("Workspace '{}' not found", workspace_name);
    }

    // Start transaction to remove workspace
    let mut tx = repo.start_transaction();
    tx.repo_mut().remove_wc_commit(ws_name);

    // Commit transaction
    let description = format!("forget workspace '{}'", workspace_name);
    tx.commit(&description)
        .context("Failed to commit workspace removal")?;

    Ok(())
}

/// Create a seed commit for fast initial publishes
///
/// The seed commit captures the initial repository state at init time.
/// When publishing root checkpoints (parent=None), we use the seed's tree
/// as the base for incremental tree conversion, making the first publish
/// as fast as subsequent publishes.
///
/// This function:
/// 1. Loads the JJ workspace
/// 2. Gets the current working copy tree (@ commit's tree)
/// 3. Creates a new "seed" commit with this tree
/// 4. Returns the commit ID for storage in JjMapping
///
/// # Arguments
///
/// * `repo_root` - The repository root containing .jj/
///
/// # Returns
///
/// The hex commit ID of the created seed commit.
///
/// # Errors
///
/// Returns error if workspace loading or commit creation fails.
pub fn create_seed_commit(repo_root: &Path) -> Result<String> {
    // Load workspace
    let workspace = load_workspace(repo_root)?;
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repo")?;

    // Get current @ commit's tree
    let wc_commit_id = Repo::view(repo.as_ref())
        .get_wc_commit_id(workspace.workspace_name())
        .ok_or_else(|| JjError::OperationFailed("No working copy commit found".to_string()))?;

    let jj_store = Repo::store(repo.as_ref());
    let wc_commit = jj_store.get_commit(&wc_commit_id)
        .context("Failed to get working copy commit")?;

    // Get the tree from working copy (tree() returns MergedTree directly in 0.36.0)
    let tree = wc_commit.tree();

    // Create seed commit with:
    // - Same tree as current @ (captures initial state)
    // - JJ root as parent (clean lineage)
    // - Descriptive message
    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();

    let root_commit_id = Repo::store(mut_repo).root_commit_id().clone();

    let seed_commit = mut_repo.new_commit(
        vec![root_commit_id],
        tree,
    )
    .set_description("Timelapse seed: initial repository state\n\nThis commit enables fast incremental publishing.")
    .write()?;

    let commit_id = seed_commit.id().hex();

    // Commit the transaction
    tx.commit("create timelapse seed commit")
        .context("Failed to commit seed commit creation")?;

    Ok(commit_id)
}

/// Create seed commit and store mapping (convenience function)
///
/// Combines `create_seed_commit` with storing the result in JjMapping.
/// This is the primary function to call during init.
///
/// # Arguments
///
/// * `repo_root` - The repository root
/// * `tl_dir` - The .tl directory for JjMapping storage
///
/// # Errors
///
/// Returns error if seed creation or mapping storage fails.
pub fn create_and_store_seed(repo_root: &Path, tl_dir: &Path) -> Result<String> {
    let commit_id = create_seed_commit(repo_root)?;

    let mapping = JjMapping::open(tl_dir)?;
    mapping.set_seed(&commit_id)?;

    Ok(commit_id)
}
