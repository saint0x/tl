//! Checkpoint materialization logic
//!
//! Convert Timelapse checkpoints into JJ commits. This involves:
//! 1. Converting Timelapse tree format to JJ tree format
//! 2. Creating JJ commits with proper metadata
//! 3. Handling checkpoint ranges (compact or expand modes)
//!
//! All operations support configurable behavior via options structs.

use anyhow::{Context, Result};
use tl_core::{Store, Tree, EntryKind};
use journal::Checkpoint;
use jj_lib::backend::ObjectId;
use std::path::PathBuf;

/// Options for commit message formatting
#[derive(Debug, Clone)]
pub struct CommitMessageOptions {
    /// Include list of changed files in commit message
    pub include_files: bool,

    /// Maximum number of files to list (rest shown as "... and N more")
    pub max_files_shown: usize,

    /// Include checkpoint metadata (timestamp, stats, etc.)
    pub include_metadata: bool,

    /// Custom message template (use {id}, {reason}, {timestamp} as placeholders)
    pub template: Option<String>,
}

impl Default for CommitMessageOptions {
    fn default() -> Self {
        Self {
            include_files: true,
            max_files_shown: 10,
            include_metadata: true,
            template: None,
        }
    }
}

/// Options for publishing checkpoints
#[derive(Debug, Clone)]
pub struct PublishOptions {
    /// Auto-pin published checkpoints with this name
    pub auto_pin: Option<String>,

    /// Commit message formatting options
    pub message_options: CommitMessageOptions,

    /// For ranges: compact (single commit) or expand (one commit per checkpoint)
    pub compact_range: bool,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            auto_pin: Some("published".to_string()),
            message_options: CommitMessageOptions::default(),
            compact_range: false, // Default to expand (preserve fine-grained history)
        }
    }
}

/// Format a commit message for a checkpoint
///
/// Supports customization via CommitMessageOptions:
/// - Custom templates with placeholders
/// - File listing (with max limit)
/// - Metadata inclusion
pub fn format_commit_message(
    checkpoint: &Checkpoint,
    options: &CommitMessageOptions,
) -> String {
    // Use custom template if provided
    if let Some(ref template) = options.template {
        return expand_template(template, checkpoint);
    }

    // Default format
    let short_id = &checkpoint.id.to_string()[..8];
    let mut msg = format!("Checkpoint {} ({:?})\n\n", short_id, checkpoint.reason);

    // File list (if enabled)
    if options.include_files && !checkpoint.touched_paths.is_empty() {
        msg.push_str("Files changed:\n");
        let files_to_show = options.max_files_shown.min(checkpoint.touched_paths.len());

        for path in checkpoint.touched_paths.iter().take(files_to_show) {
            msg.push_str(&format!("  - {}\n", path.display()));
        }

        if checkpoint.touched_paths.len() > files_to_show {
            msg.push_str(&format!(
                "  ... and {} more\n",
                checkpoint.touched_paths.len() - files_to_show
            ));
        }
        msg.push('\n');
    }

    // Metadata (if enabled)
    if options.include_metadata {
        msg.push_str(&format!("Timestamp: {}\n", checkpoint.ts_unix_ms));
        msg.push_str(&format!("Files: {}\n", checkpoint.meta.files_changed));
        msg.push_str(&format!("Added: {} bytes\n", checkpoint.meta.bytes_added));
        msg.push_str(&format!("Removed: {} bytes\n", checkpoint.meta.bytes_removed));
    }

    msg
}

/// Expand template string with checkpoint data
///
/// Supported placeholders:
/// - {id} - Full checkpoint ID
/// - {short_id} - First 8 chars of ID
/// - {reason} - Checkpoint reason
/// - {timestamp} - Unix timestamp in milliseconds
/// - {files_changed} - Number of files changed
/// - {bytes_added} - Bytes added
/// - {bytes_removed} - Bytes removed
fn expand_template(template: &str, checkpoint: &Checkpoint) -> String {
    let short_id = &checkpoint.id.to_string()[..8];

    template
        .replace("{id}", &checkpoint.id.to_string())
        .replace("{short_id}", short_id)
        .replace("{reason}", &format!("{:?}", checkpoint.reason))
        .replace("{timestamp}", &checkpoint.ts_unix_ms.to_string())
        .replace("{files_changed}", &checkpoint.meta.files_changed.to_string())
        .replace("{bytes_added}", &checkpoint.meta.bytes_added.to_string())
        .replace("{bytes_removed}", &checkpoint.meta.bytes_removed.to_string())
}

/// Convert Timelapse tree to JJ tree
///
/// Converts a Timelapse tree representation to a JJ tree using native jj-lib APIs.
/// This involves:
/// 1. Iterate over Timelapse tree entries
/// 2. Read blob content from Timelapse store
/// 3. Write blobs to JJ backend
/// 4. Build JJ tree with proper TreeValue types (File, Symlink)
/// 5. Write the tree hierarchy to backend
pub fn convert_tree_to_jj(
    tl_tree: &Tree,
    store: &Store,
    jj_store: &std::sync::Arc<jj_lib::store::Store>,
) -> Result<jj_lib::backend::TreeId> {
    use jj_lib::repo_path::{RepoPath, RepoPathBuf};
    use jj_lib::backend::TreeValue;
    use jj_lib::tree_builder::TreeBuilder;

    // Create a TreeBuilder starting from empty tree
    let empty_tree_id = jj_store.empty_tree_id().clone();
    let mut tree_builder = TreeBuilder::new(jj_store.clone(), empty_tree_id);

    // Iterate Timelapse tree entries
    for (path_bytes, entry) in tl_tree.entries_with_paths() {
        let path_str = std::str::from_utf8(path_bytes)
            .context("Invalid UTF-8 in file path")?;

        // Skip protected directories
        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") || path_str.starts_with(".jj/") {
            continue;
        }

        // Read blob content from Timelapse store
        let content = store.blob_store().read_blob(entry.blob_hash)
            .with_context(|| format!("Failed to read blob for {}", path_str))?;

        // Convert path to RepoPath
        let repo_path = RepoPath::from_internal_string(path_str);

        // Write blob to JJ store and get file ID/symlink ID
        let tree_value = match entry.kind {
            EntryKind::File | EntryKind::ExecutableFile => {
                // Write file to store
                let mut cursor = std::io::Cursor::new(&content);
                let file_id = jj_store.write_file(&repo_path, &mut cursor)
                    .with_context(|| format!("Failed to write file to JJ store: {}", path_str))?;

                // Check if executable
                let executable = matches!(entry.kind, EntryKind::ExecutableFile) || (entry.mode & 0o111 != 0);
                TreeValue::File {
                    id: file_id,
                    executable,
                }
            }
            EntryKind::Symlink => {
                // Convert content to string for symlink target
                let target = String::from_utf8(content)
                    .context("Symlink target is not valid UTF-8")?;

                // Write symlink to store
                let symlink_id = jj_store.write_symlink(&repo_path, &target)
                    .with_context(|| format!("Failed to write symlink to JJ store: {}", path_str))?;

                TreeValue::Symlink(symlink_id)
            }
            EntryKind::Tree => {
                // Skip tree entries - TreeBuilder handles directory structure automatically
                continue;
            }
        };

        // Add to tree builder (it handles nested paths automatically)
        tree_builder.set(RepoPathBuf::from_internal_string(path_str), tree_value);
    }

    // Write the entire tree hierarchy and return root tree ID
    let tree_id = tree_builder.write_tree();

    Ok(tree_id)
}

/// Convert Timelapse tree to JJ tree (incremental mode)
///
/// When a parent JJ tree is available, only processes changed paths instead of
/// the entire tree. This provides O(changed_files) performance instead of O(total_files).
///
/// Falls back to full conversion when:
/// - No parent tree is provided
/// - touched_paths is empty (safety fallback)
pub fn convert_tree_to_jj_incremental(
    tl_tree: &Tree,
    store: &Store,
    jj_store: &std::sync::Arc<jj_lib::store::Store>,
    touched_paths: &[PathBuf],
    parent_jj_tree_id: Option<&jj_lib::backend::TreeId>,
) -> Result<jj_lib::backend::TreeId> {
    use jj_lib::repo_path::{RepoPath, RepoPathBuf};
    use jj_lib::backend::TreeValue;
    use jj_lib::tree_builder::TreeBuilder;

    // Fall back to full conversion if no parent tree or no touched paths
    let base_tree_id = match parent_jj_tree_id {
        Some(tree_id) if !touched_paths.is_empty() => tree_id.clone(),
        _ => {
            // Full conversion needed
            return convert_tree_to_jj(tl_tree, store, jj_store);
        }
    };

    // Create TreeBuilder starting from parent tree
    let mut tree_builder = TreeBuilder::new(jj_store.clone(), base_tree_id);

    // Process only the touched paths
    for path in touched_paths {
        let path_str = path.to_string_lossy().into_owned();

        // Skip protected directories
        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") || path_str.starts_with(".jj/") {
            continue;
        }

        // Check if file exists in the new tree
        if let Some(entry) = tl_tree.get(path) {
            // File exists - add or update it
            let content = store.blob_store().read_blob(entry.blob_hash)
                .with_context(|| format!("Failed to read blob for {}", path_str))?;

            let repo_path = RepoPath::from_internal_string(&path_str);

            let tree_value = match entry.kind {
                EntryKind::File | EntryKind::ExecutableFile => {
                    let mut cursor = std::io::Cursor::new(&content);
                    let file_id = jj_store.write_file(&repo_path, &mut cursor)
                        .with_context(|| format!("Failed to write file to JJ store: {}", path_str))?;

                    let executable = matches!(entry.kind, EntryKind::ExecutableFile) || (entry.mode & 0o111 != 0);
                    TreeValue::File {
                        id: file_id,
                        executable,
                    }
                }
                EntryKind::Symlink => {
                    let target = String::from_utf8(content)
                        .context("Symlink target is not valid UTF-8")?;

                    let symlink_id = jj_store.write_symlink(&repo_path, &target)
                        .with_context(|| format!("Failed to write symlink to JJ store: {}", path_str))?;

                    TreeValue::Symlink(symlink_id)
                }
                EntryKind::Tree => {
                    // Skip tree entries - TreeBuilder handles directories automatically
                    continue;
                }
            };

            tree_builder.set(RepoPathBuf::from_internal_string(path_str.clone()), tree_value);
        } else {
            // File doesn't exist in new tree - it was deleted
            let repo_path = RepoPathBuf::from_internal_string(path_str);
            tree_builder.remove(repo_path);
        }
    }

    // Write the tree and return
    let tree_id = tree_builder.write_tree();
    Ok(tree_id)
}

/// Publish a single checkpoint to JJ
///
/// Creates a JJ commit from the checkpoint using native jj-lib APIs.
/// This involves:
/// 1. Start transaction on workspace
/// 2. Convert Timelapse tree to JJ tree
/// 3. Determine parent commits (from mapping or current @)
/// 4. Build commit with CommitBuilder
/// 5. Commit transaction
/// 6. Store bidirectional mapping
/// 7. Auto-pin if configured
pub fn publish_checkpoint(
    checkpoint: &Checkpoint,
    store: &Store,
    workspace: &mut jj_lib::workspace::Workspace,
    mapping: &crate::mapping::JjMapping,
    options: &PublishOptions,
) -> Result<String> {
    use jj_lib::settings::UserSettings;
    use jj_lib::repo::Repo;  // Import trait for methods
    use jj_lib::backend::MergedTreeId;

    // Get user settings from workspace
    // Create default settings if workspace doesn't have them
    let config = config::Config::builder().build()
        .context("Failed to create config")?;
    let user_settings = UserSettings::from_config(config);

    // Load the repo at head
    let repo = workspace.repo_loader().load_at_head(&user_settings)
        .context("Failed to load repo")?;

    // Start transaction
    let mut tx = repo.start_transaction(&user_settings);
    let mut_repo = tx.mut_repo();
    let jj_store = Repo::store(mut_repo);

    // Determine parent commits and get parent's JJ tree for incremental conversion
    let (parent_ids, parent_jj_tree_id): (Vec<_>, Option<jj_lib::backend::TreeId>) =
        if let Some(parent_cp_id) = checkpoint.parent {
            // Parent checkpoint exists - check if it's published to JJ
            if let Some(jj_commit_id_str) = mapping.get_jj_commit(parent_cp_id)? {
                // Parent is published - get its commit and tree
                let parent_commit_id = jj_lib::backend::CommitId::from_hex(&jj_commit_id_str);

                // Try to get parent's tree ID for incremental conversion
                let parent_tree_id = match jj_store.get_commit(&parent_commit_id) {
                    Ok(parent_commit) => {
                        match parent_commit.tree_id() {
                            MergedTreeId::Legacy(tree_id) => Some(tree_id.clone()),
                            MergedTreeId::Merge(_) => None, // Merge trees need full conversion
                        }
                    }
                    Err(_) => None, // Can't get commit, fall back to full conversion
                };

                (vec![parent_commit_id], parent_tree_id)
            } else {
                // Parent not published, use current @, no incremental possible
                let wc_commit_id = Repo::view(mut_repo).get_wc_commit_id(workspace.workspace_id())
                    .ok_or_else(|| anyhow::anyhow!("No working copy commit found"))?;
                (vec![wc_commit_id.clone()], None)
            }
        } else {
            // Root checkpoint - use root commit as parent, no incremental possible
            (vec![Repo::store(mut_repo).root_commit_id().clone()], None)
        };

    // Convert Timelapse tree to JJ tree (incremental if possible)
    let tree = store.read_tree(checkpoint.root_tree)
        .context("Failed to read checkpoint tree")?;
    let jj_tree_id = convert_tree_to_jj_incremental(
        &tree,
        store,
        jj_store,
        &checkpoint.touched_paths,
        parent_jj_tree_id.as_ref(),
    )?;

    // Format commit message
    let commit_message = format_commit_message(checkpoint, &options.message_options);

    // Build commit with native API
    // Convert TreeId to MergedTreeId (single, non-merge tree)
    let merged_tree_id = jj_lib::backend::MergedTreeId::Legacy(jj_tree_id);

    let commit = mut_repo.new_commit(
        &user_settings,
        parent_ids,
        merged_tree_id,
    )
    .set_description(commit_message)
    .write()?;

    let commit_id = commit.id().hex();

    // Update working copy pointer (workspace_id() returns &WorkspaceId, so clone it)
    mut_repo.set_wc_commit(workspace.workspace_id().clone(), commit.id().clone())?;

    // Commit transaction (no need to update working copy - it's handled internally)
    let _committed_tx = tx.commit("publish checkpoint");

    // Store bidirectional mapping
    mapping.set(checkpoint.id, &commit_id)
        .context("Failed to store checkpoint mapping")?;
    mapping.set_reverse(&commit_id, checkpoint.id)
        .context("Failed to store reverse mapping")?;

    // Auto-pin if configured
    if let Some(ref pin_name) = options.auto_pin {
        // Note: Auto-pinning would require integration with pin manager
        // For now, we'll skip this as it's optional
        // TODO: Integrate with PinManager once available
        let _ = pin_name; // Silence unused warning
    }

    Ok(commit_id)
}

/// Publish a range of checkpoints to JJ
///
/// Behavior depends on options.compact_range:
/// - If true: Create single JJ commit from end checkpoint (squash)
/// - If false: Create one JJ commit per checkpoint (preserve history)
pub fn publish_range(
    checkpoints: Vec<Checkpoint>,
    store: &Store,
    workspace: &mut jj_lib::workspace::Workspace,
    mapping: &crate::mapping::JjMapping,
    options: &PublishOptions,
) -> Result<Vec<String>> {
    if options.compact_range {
        // Compact mode: only publish the last checkpoint
        if let Some(last) = checkpoints.last() {
            let commit_id = publish_checkpoint(last, store, workspace, mapping, options)?;
            Ok(vec![commit_id])
        } else {
            Ok(vec![])
        }
    } else {
        // Expand mode: publish each checkpoint
        let mut commit_ids = Vec::new();
        for checkpoint in checkpoints {
            let commit_id = publish_checkpoint(&checkpoint, store, workspace, mapping, options)?;
            commit_ids.push(commit_id);
        }
        Ok(commit_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use journal::{CheckpointMeta, CheckpointReason};
    use std::path::PathBuf;
    use tl_core::Sha1Hash;
    use ulid::Ulid;

    fn test_checkpoint() -> Checkpoint {
        Checkpoint {
            id: Ulid::new(),
            parent: None,
            root_tree: Sha1Hash::from_bytes([0u8; 20]),
            ts_unix_ms: 1704067200000,
            reason: CheckpointReason::Manual,
            touched_paths: vec![
                PathBuf::from("file1.txt"),
                PathBuf::from("file2.txt"),
            ],
            meta: CheckpointMeta {
                files_changed: 2,
                bytes_added: 1024,
                bytes_removed: 512,
            },
        }
    }

    #[test]
    fn test_format_commit_message_default() {
        let cp = test_checkpoint();
        let options = CommitMessageOptions::default();
        let msg = format_commit_message(&cp, &options);

        // Should include short ID
        let short_id = &cp.id.to_string()[..8];
        assert!(msg.contains(short_id));

        // Should include reason
        assert!(msg.contains("Manual"));

        // Should include files
        assert!(msg.contains("file1.txt"));
        assert!(msg.contains("file2.txt"));

        // Should include metadata
        assert!(msg.contains("Timestamp:"));
        assert!(msg.contains("Files: 2"));
    }

    #[test]
    fn test_format_commit_message_no_files() {
        let cp = test_checkpoint();
        let mut options = CommitMessageOptions::default();
        options.include_files = false;

        let msg = format_commit_message(&cp, &options);

        // Should not include file list
        assert!(!msg.contains("file1.txt"));
    }

    #[test]
    fn test_format_commit_message_custom_template() {
        let cp = test_checkpoint();
        let mut options = CommitMessageOptions::default();
        options.template = Some("Checkpoint {short_id}: {reason}".to_string());

        let msg = format_commit_message(&cp, &options);

        let short_id = &cp.id.to_string()[..8];
        assert_eq!(msg, format!("Checkpoint {}: Manual", short_id));
    }

    #[test]
    fn test_expand_template() {
        let cp = test_checkpoint();
        let template = "ID: {short_id}, Files: {files_changed}, Reason: {reason}";
        let expanded = expand_template(template, &cp);

        assert!(expanded.contains("ID:"));
        assert!(expanded.contains("Files: 2"));
        assert!(expanded.contains("Reason: Manual"));
    }

    #[test]
    fn test_publish_options_defaults() {
        let options = PublishOptions::default();
        assert_eq!(options.auto_pin, Some("published".to_string()));
        assert!(!options.compact_range); // Should expand by default
        assert!(options.message_options.include_files);
    }
}
