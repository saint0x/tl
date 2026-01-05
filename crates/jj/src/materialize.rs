//! Checkpoint materialization logic
//!
//! Convert Timelapse checkpoints into JJ commits. This involves:
//! 1. Converting Timelapse tree format to JJ tree format
//! 2. Creating JJ commits with proper metadata
//! 3. Handling checkpoint ranges (compact or expand modes)
//!
//! All operations support configurable behavior via options structs.
//!
//! Performance optimizations:
//! - Skip blob read/write for objects that already exist in .git/objects/
//! - Construct FileId directly from TL hash (same SHA-1 format as Git)
//! - Single sled flush at end of publish operations

use anyhow::{Context, Result};
use jj_lib::backend::CopyId;
use jj_lib::object_id::ObjectId;
use journal::Checkpoint;
use pollster::FutureExt as _;
use std::path::{Path, PathBuf};
use tl_core::{EntryKind, Sha1Hash, Store, Tree};

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

    /// Pre-computed accumulated touched_paths (union of all paths from checkpoint
    /// back to nearest published ancestor). If provided, used instead of tree diff.
    /// This optimization avoids O(all_files) tree comparison when parent isn't published.
    pub accumulated_paths: Option<Vec<PathBuf>>,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            auto_pin: Some("published".to_string()),
            message_options: CommitMessageOptions::default(),
            compact_range: false, // Default to expand (preserve fine-grained history)
            accumulated_paths: None, // Computed by caller when needed
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

/// Check if a Git object already exists in .git/objects/
///
/// This enables skipping the read/decompress/recompress cycle for blobs
/// that are already in Git's object store. TL uses Git-compatible blob
/// format with identical SHA-1 hashing, so the hashes match exactly.
#[inline]
fn git_object_exists(repo_root: &Path, blob_hash: &Sha1Hash) -> bool {
    let hex = blob_hash.to_hex();
    let git_path = repo_root
        .join(".git/objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    git_path.exists()
}

/// Construct a JJ FileId directly from a TL SHA-1 hash
///
/// TL uses Git-compatible blob format, so the SHA-1 hashes are identical.
/// This allows us to create FileId without reading/writing the blob content.
#[inline]
fn file_id_from_hash(hash: &Sha1Hash) -> jj_lib::backend::FileId {
    jj_lib::backend::FileId::new(hash.as_bytes().to_vec())
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
///
/// Performance: Skips blob read/write for objects already in .git/objects/
pub fn convert_tree_to_jj(
    tl_tree: &Tree,
    store: &Store,
    jj_store: &std::sync::Arc<jj_lib::store::Store>,
    repo_root: &Path,
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

        // Convert path to RepoPath (returns Result in 0.36.0)
        let repo_path = RepoPath::from_internal_string(path_str)
            .with_context(|| format!("Invalid repo path: {}", path_str))?;

        // Write blob to JJ store and get file ID/symlink ID
        let tree_value = match entry.kind {
            EntryKind::File | EntryKind::ExecutableFile => {
                // OPTIMIZATION: Check if blob already exists in .git/objects/
                // TL uses Git-compatible SHA-1 hashing, so we can construct FileId directly
                let file_id = if git_object_exists(repo_root, &entry.blob_hash) {
                    // Fast path: construct FileId directly from hash
                    file_id_from_hash(&entry.blob_hash)
                } else {
                    // Slow path: read from TL, write to JJ
                    let content = store.blob_store().read_blob(entry.blob_hash)
                        .with_context(|| format!("Failed to read blob for {}", path_str))?;
                    let mut cursor = std::io::Cursor::new(&content);
                    jj_store.write_file(repo_path, &mut cursor)
                        .block_on()
                        .with_context(|| format!("Failed to write file to JJ store: {}", path_str))?
                };

                // Check if executable
                let executable = matches!(entry.kind, EntryKind::ExecutableFile) || (entry.mode & 0o111 != 0);
                TreeValue::File {
                    id: file_id,
                    executable,
                    copy_id: CopyId::placeholder(), // No copy tracking
                }
            }
            EntryKind::Symlink => {
                // Symlinks need content for target - always read
                let content = store.blob_store().read_blob(entry.blob_hash)
                    .with_context(|| format!("Failed to read blob for {}", path_str))?;
                let target = String::from_utf8(content)
                    .context("Symlink target is not valid UTF-8")?;

                // Write symlink to store (async with block_on)
                let symlink_id = jj_store.write_symlink(repo_path, &target)
                    .block_on()
                    .with_context(|| format!("Failed to write symlink to JJ store: {}", path_str))?;

                TreeValue::Symlink(symlink_id)
            }
            EntryKind::Tree => {
                // Skip tree entries - TreeBuilder handles directory structure automatically
                continue;
            }
        };

        // Add to tree builder (it handles nested paths automatically)
        // RepoPathBuf::from_internal_string returns Result in 0.36.0
        let repo_path_buf = RepoPathBuf::from_internal_string(path_str)
            .with_context(|| format!("Invalid repo path: {}", path_str))?;
        tree_builder.set(repo_path_buf, tree_value);
    }

    // Write the entire tree hierarchy and return root tree ID
    // write_tree() returns Result in 0.36.0
    let tree_id = tree_builder.write_tree()
        .context("Failed to write tree")?;

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
///
/// Performance: Skips blob read/write for objects already in .git/objects/
pub fn convert_tree_to_jj_incremental(
    tl_tree: &Tree,
    store: &Store,
    jj_store: &std::sync::Arc<jj_lib::store::Store>,
    touched_paths: &[PathBuf],
    parent_jj_tree_id: Option<&jj_lib::backend::TreeId>,
    repo_root: &Path,
) -> Result<jj_lib::backend::TreeId> {
    use jj_lib::repo_path::{RepoPath, RepoPathBuf};
    use jj_lib::backend::TreeValue;
    use jj_lib::tree_builder::TreeBuilder;

    // Fall back to full conversion if no parent tree or no touched paths
    let base_tree_id = match parent_jj_tree_id {
        Some(tree_id) if !touched_paths.is_empty() => tree_id.clone(),
        _ => {
            // Full conversion needed
            return convert_tree_to_jj(tl_tree, store, jj_store, repo_root);
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
            // Convert path to RepoPath (returns Result in 0.36.0)
            let repo_path = RepoPath::from_internal_string(&path_str)
                .with_context(|| format!("Invalid repo path: {}", path_str))?;

            let tree_value = match entry.kind {
                EntryKind::File | EntryKind::ExecutableFile => {
                    // OPTIMIZATION: Check if blob already exists in .git/objects/
                    let file_id = if git_object_exists(repo_root, &entry.blob_hash) {
                        // Fast path: construct FileId directly from hash
                        file_id_from_hash(&entry.blob_hash)
                    } else {
                        // Slow path: read from TL, write to JJ
                        let content = store.blob_store().read_blob(entry.blob_hash)
                            .with_context(|| format!("Failed to read blob for {}", path_str))?;
                        let mut cursor = std::io::Cursor::new(&content);
                        jj_store.write_file(repo_path, &mut cursor)
                            .block_on()
                            .with_context(|| format!("Failed to write file to JJ store: {}", path_str))?
                    };

                    let executable = matches!(entry.kind, EntryKind::ExecutableFile) || (entry.mode & 0o111 != 0);
                    TreeValue::File {
                        id: file_id,
                        executable,
                        copy_id: CopyId::placeholder(), // No copy tracking
                    }
                }
                EntryKind::Symlink => {
                    // Symlinks need content for target - always read
                    let content = store.blob_store().read_blob(entry.blob_hash)
                        .with_context(|| format!("Failed to read blob for {}", path_str))?;
                    let target = String::from_utf8(content)
                        .context("Symlink target is not valid UTF-8")?;

                    let symlink_id = jj_store.write_symlink(repo_path, &target)
                        .block_on()
                        .with_context(|| format!("Failed to write symlink to JJ store: {}", path_str))?;

                    TreeValue::Symlink(symlink_id)
                }
                EntryKind::Tree => {
                    // Skip tree entries - TreeBuilder handles directories automatically
                    continue;
                }
            };

            // Create RepoPathBuf for tree_builder (returns Result in 0.36.0)
            let repo_path_buf = RepoPathBuf::from_internal_string(&path_str)
                .with_context(|| format!("Invalid repo path: {}", path_str))?;
            tree_builder.set(repo_path_buf, tree_value);
        } else {
            // File doesn't exist in new tree - it was deleted
            let repo_path = RepoPathBuf::from_internal_string(&path_str)
                .with_context(|| format!("Invalid repo path for deletion: {}", path_str))?;
            tree_builder.remove(repo_path);
        }
    }

    // Write the tree and return (returns Result in 0.36.0)
    let tree_id = tree_builder.write_tree()
        .context("Failed to write tree")?;
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
/// 7. Flush mapping database
/// 8. Auto-pin if configured
pub fn publish_checkpoint(
    checkpoint: &Checkpoint,
    store: &Store,
    workspace: &mut jj_lib::workspace::Workspace,
    mapping: &crate::mapping::JjMapping,
    options: &PublishOptions,
    repo_root: &Path,
) -> Result<String> {
    use jj_lib::merged_tree::MergedTree;
    use jj_lib::repo::Repo;  // Import trait for methods

    // Load the repo at head (no longer takes user_settings in 0.36.0)
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repo")?;

    // Get user settings for author/committer signature
    let user_settings = crate::create_user_settings()
        .context("Failed to create user settings")?;

    // Start transaction (no longer takes user_settings in 0.36.0)
    let mut tx = repo.start_transaction();
    let mut_repo = tx.repo_mut();
    let jj_store = Repo::store(mut_repo);

    // Load the current checkpoint's tree first (needed for tree diff computation)
    let tree = store.read_tree(checkpoint.root_tree)
        .context("Failed to read checkpoint tree")?;

    // Determine parent commits and get parent's JJ tree for incremental conversion
    // Also track whether we're using true parent (touched_paths valid) or seed fallback (need tree diff)
    // In 0.36.0: tree_ids() returns &Merge<TreeId>, use as_resolved() to get single tree
    let (parent_ids, parent_jj_tree_id, use_true_parent): (Vec<_>, Option<jj_lib::backend::TreeId>, bool) =
        if let Some(parent_cp_id) = checkpoint.parent {
            // Parent checkpoint exists - check if it's published to JJ
            if let Some(jj_commit_id_str) = mapping.get_jj_commit(parent_cp_id)? {
                // Parent is published - get its commit and tree
                // Use hex::decode + CommitId::new to avoid lifetime issues
                let parent_commit_id = jj_lib::backend::CommitId::new(
                    hex::decode(&jj_commit_id_str).context("Invalid parent commit hex")?
                );

                // Try to get parent's tree ID for incremental conversion
                let parent_tree_id = match jj_store.get_commit(&parent_commit_id) {
                    Ok(parent_commit) => {
                        // tree_ids() returns &Merge<TreeId>, as_resolved() gives Option<&TreeId>
                        parent_commit.tree_ids().as_resolved().cloned()
                    }
                    Err(_) => None, // Can't get commit, fall back to full conversion
                };

                (vec![parent_commit_id], parent_tree_id, true) // True parent: touched_paths valid
            } else {
                // Parent not published - use seed tree as fallback (need tree diff)
                let wc_commit_id = Repo::view(mut_repo).get_wc_commit_id(workspace.workspace_name())
                    .ok_or_else(|| anyhow::anyhow!("No working copy commit found"))?;

                let seed_tree_id = mapping.get_seed()?.and_then(|seed_commit_id_str| {
                    // Use hex::decode + CommitId::new to avoid lifetime issues
                    let seed_commit_id = jj_lib::backend::CommitId::new(
                        hex::decode(&seed_commit_id_str).ok()?
                    );
                    match jj_store.get_commit(&seed_commit_id) {
                        Ok(seed_commit) => {
                            seed_commit.tree_ids().as_resolved().cloned()
                        }
                        Err(_) => None,
                    }
                });
                (vec![wc_commit_id.clone()], seed_tree_id, false) // Seed fallback: need tree diff
            }
        } else {
            // Root checkpoint - try to use seed commit for incremental conversion
            let seed_tree_id = mapping.get_seed()?.and_then(|seed_commit_id_str| {
                // Use hex::decode + CommitId::new to avoid lifetime issues
                let seed_commit_id = jj_lib::backend::CommitId::new(
                    hex::decode(&seed_commit_id_str).ok()?
                );
                match jj_store.get_commit(&seed_commit_id) {
                    Ok(seed_commit) => {
                        seed_commit.tree_ids().as_resolved().cloned()
                    }
                    Err(_) => None,
                }
            });
            (vec![Repo::store(mut_repo).root_commit_id().clone()], seed_tree_id, false) // Seed fallback: need tree diff
        };

    // Determine which paths to process for incremental conversion
    // - If true parent: use checkpoint.touched_paths (paths changed from parent)
    // - If accumulated_paths provided: use pre-computed union of touched_paths
    // - Otherwise: fall back to tree diff (slow, O(all_files))
    let effective_touched_paths: Vec<PathBuf> = if use_true_parent {
        // True parent is published - touched_paths are correct
        checkpoint.touched_paths.clone()
    } else if let Some(ref paths) = options.accumulated_paths {
        // Use pre-computed accumulated paths (fast, O(changed_files))
        paths.clone()
    } else if parent_jj_tree_id.is_some() {
        // Legacy fallback - compute full tree diff (slow, O(all_files))
        // This path is only taken when caller doesn't pre-compute accumulated_paths
        let seed_tree = load_seed_tree(store, mapping)?;
        compute_tree_diff_paths(&seed_tree, &tree)
    } else {
        // No seed available - full conversion will be used
        Vec::new()
    };

    // Convert Timelapse tree to JJ tree (incremental if possible)
    let jj_tree_id = convert_tree_to_jj_incremental(
        &tree,
        store,
        jj_store,
        &effective_touched_paths,
        parent_jj_tree_id.as_ref(),
        repo_root,
    )?;

    // Format commit message
    let commit_message = format_commit_message(checkpoint, &options.message_options);

    // Build commit with native API
    // In 0.36.0: new_commit takes MergedTree, not MergedTreeId
    // Create MergedTree::resolved from the single tree ID
    let merged_tree = MergedTree::resolved(jj_store.clone(), jj_tree_id);

    // Get author/committer signature from user settings
    let author_signature = user_settings.signature();

    let commit = mut_repo.new_commit(
        parent_ids,
        merged_tree,
    )
    .set_description(commit_message)
    .set_author(author_signature.clone())
    .set_committer(author_signature)
    .write()?;

    let commit_id = commit.id().hex();

    // Update working copy pointer (workspace_name() returns &WorkspaceName)
    mut_repo.set_wc_commit(workspace.workspace_name().to_owned(), commit.id().clone())?;

    // Commit transaction (returns Result in 0.36.0)
    tx.commit("publish checkpoint")
        .context("Failed to commit transaction")?;

    // Store bidirectional mapping
    mapping.set(checkpoint.id, &commit_id)
        .context("Failed to store checkpoint mapping")?;
    mapping.set_reverse(&commit_id, checkpoint.id)
        .context("Failed to store reverse mapping")?;

    // Flush mapping database to disk (single flush instead of per-write)
    mapping.flush()
        .context("Failed to flush mapping database")?;

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
///
/// Performance optimization: Uses a single transaction for all checkpoints
/// to avoid repeated repo loading and transaction overhead.
pub fn publish_range(
    checkpoints: Vec<Checkpoint>,
    store: &Store,
    repo_root: &Path,
    mapping: &crate::mapping::JjMapping,
    options: &PublishOptions,
) -> Result<Vec<String>> {
    use jj_lib::merged_tree::MergedTree;
    use jj_lib::repo::Repo;

    if checkpoints.is_empty() {
        return Ok(vec![]);
    }

    // Load workspace here to share between compact and expand modes
    let mut workspace = crate::load_workspace(repo_root)?;

    // Compact mode: only publish the last checkpoint
    if options.compact_range {
        if let Some(last) = checkpoints.last() {
            let commit_id = publish_checkpoint(last, store, &mut workspace, mapping, options, repo_root)?;
            return Ok(vec![commit_id]);
        }
        return Ok(vec![]);
    }

    // Expand mode: publish all checkpoints in a SINGLE transaction
    // This avoids repeated repo loading and transaction overhead

    // Load repo ONCE
    let repo = workspace.repo_loader().load_at_head()
        .context("Failed to load repo")?;
    let workspace_name = workspace.workspace_name().to_owned();

    // Get user settings for author/committer signature
    let user_settings = crate::create_user_settings()
        .context("Failed to create user settings")?;
    let author_signature = user_settings.signature();

    // Start transaction ONCE
    let mut tx = repo.start_transaction();

    let mut commit_ids = Vec::new();
    let mut last_commit: Option<jj_lib::commit::Commit> = None;

    for checkpoint in &checkpoints {
        // Get fresh references each iteration to satisfy borrow checker
        let mut_repo = tx.repo_mut();
        let jj_store = Repo::store(mut_repo).clone(); // Clone the Arc

        // Load the checkpoint's tree
        let tree = store.read_tree(checkpoint.root_tree)
            .context("Failed to read checkpoint tree")?;

        // Determine parent commits and tree for incremental conversion
        let (parent_ids, parent_jj_tree_id, use_true_parent) = determine_parents(
            checkpoint,
            mapping,
            &jj_store,
            tx.repo_mut(),
            &workspace_name,
            last_commit.as_ref(),
        )?;

        // Determine effective touched paths
        // - If true parent: use checkpoint.touched_paths (paths changed from parent)
        // - If accumulated_paths provided: use pre-computed union of touched_paths
        // - Otherwise: fall back to tree diff (slow, O(all_files))
        let effective_touched_paths: Vec<PathBuf> = if use_true_parent {
            checkpoint.touched_paths.clone()
        } else if let Some(ref paths) = options.accumulated_paths {
            // Use pre-computed accumulated paths (fast, O(changed_files))
            paths.clone()
        } else if parent_jj_tree_id.is_some() {
            // Legacy fallback - compute full tree diff (slow, O(all_files))
            let seed_tree = load_seed_tree(store, mapping)?;
            compute_tree_diff_paths(&seed_tree, &tree)
        } else {
            Vec::new()
        };

        // Convert tree (incremental if possible)
        // OPTIMIZATION: Pass repo_root to enable git_object_exists check
        let jj_tree_id = convert_tree_to_jj_incremental(
            &tree,
            store,
            &jj_store,
            &effective_touched_paths,
            parent_jj_tree_id.as_ref(),
            repo_root,
        )?;

        // Format commit message
        let commit_message = format_commit_message(checkpoint, &options.message_options);

        // Build commit - get fresh mut_repo reference
        let mut_repo = tx.repo_mut();
        let merged_tree = MergedTree::resolved(jj_store.clone(), jj_tree_id);
        let commit = mut_repo.new_commit(parent_ids, merged_tree)
            .set_description(commit_message)
            .set_author(author_signature.clone())
            .set_committer(author_signature.clone())
            .write()?;

        let commit_id = commit.id().hex();

        // Update working copy pointer
        mut_repo.set_wc_commit(workspace_name.clone(), commit.id().clone())?;

        // Store bidirectional mapping (no per-write flush - single flush at end)
        mapping.set(checkpoint.id, &commit_id)
            .context("Failed to store checkpoint mapping")?;
        mapping.set_reverse(&commit_id, checkpoint.id)
            .context("Failed to store reverse mapping")?;

        last_commit = Some(commit);
        commit_ids.push(commit_id);
    }

    // Commit transaction ONCE at the end
    tx.commit("publish checkpoints")
        .context("Failed to commit transaction")?;

    // Flush mapping database to disk (single flush instead of per-write)
    mapping.flush()
        .context("Failed to flush mapping database")?;

    Ok(commit_ids)
}

/// Determine parent commits and tree ID for a checkpoint being published
///
/// Returns (parent_commit_ids, parent_tree_id, use_true_parent)
fn determine_parents(
    checkpoint: &Checkpoint,
    mapping: &crate::mapping::JjMapping,
    jj_store: &std::sync::Arc<jj_lib::store::Store>,
    mut_repo: &jj_lib::repo::MutableRepo,
    workspace_name: &jj_lib::ref_name::WorkspaceName,
    last_commit: Option<&jj_lib::commit::Commit>,
) -> Result<(Vec<jj_lib::backend::CommitId>, Option<jj_lib::backend::TreeId>, bool)> {
    use jj_lib::repo::Repo;

    // If we just published a commit in this batch, use it as parent
    if let Some(prev_commit) = last_commit {
        let parent_tree_id = prev_commit.tree_ids().as_resolved().cloned();
        return Ok((vec![prev_commit.id().clone()], parent_tree_id, true));
    }

    // Check if checkpoint has a parent that's already published
    if let Some(parent_cp_id) = checkpoint.parent {
        if let Some(jj_commit_id_str) = mapping.get_jj_commit(parent_cp_id)? {
            let parent_commit_id = jj_lib::backend::CommitId::new(
                hex::decode(&jj_commit_id_str).context("Invalid parent commit hex")?
            );

            let parent_tree_id = match jj_store.get_commit(&parent_commit_id) {
                Ok(parent_commit) => parent_commit.tree_ids().as_resolved().cloned(),
                Err(_) => None,
            };

            return Ok((vec![parent_commit_id], parent_tree_id, true));
        }

        // Parent not published - use seed tree as fallback
        let wc_commit_id = Repo::view(mut_repo).get_wc_commit_id(workspace_name)
            .ok_or_else(|| anyhow::anyhow!("No working copy commit found"))?;

        let seed_tree_id = mapping.get_seed()?.and_then(|seed_commit_id_str| {
            let seed_commit_id = jj_lib::backend::CommitId::new(
                hex::decode(&seed_commit_id_str).ok()?
            );
            match jj_store.get_commit(&seed_commit_id) {
                Ok(seed_commit) => seed_commit.tree_ids().as_resolved().cloned(),
                Err(_) => None,
            }
        });

        return Ok((vec![wc_commit_id.clone()], seed_tree_id, false));
    }

    // Root checkpoint - use seed commit
    let seed_tree_id = mapping.get_seed()?.and_then(|seed_commit_id_str| {
        let seed_commit_id = jj_lib::backend::CommitId::new(
            hex::decode(&seed_commit_id_str).ok()?
        );
        match jj_store.get_commit(&seed_commit_id) {
            Ok(seed_commit) => seed_commit.tree_ids().as_resolved().cloned(),
            Err(_) => None,
        }
    });

    Ok((vec![Repo::store(mut_repo).root_commit_id().clone()], seed_tree_id, false))
}

/// Load the seed commit's tree from TL storage
///
/// The seed tree represents the initial repository state at init time.
/// Used for computing tree diff when parent checkpoint isn't published.
fn load_seed_tree(
    store: &Store,
    mapping: &crate::mapping::JjMapping,
) -> Result<Tree> {
    // Get seed checkpoint tree hash
    // Note: We store the seed's root_tree hash during init
    // For now, we return an empty tree as fallback - the actual seed tree
    // would need to be stored during init

    // Check if we have a seed tree stored
    if let Some(seed_tree_hash) = mapping.get_seed_tree()? {
        // Load the tree from store
        let hash = tl_core::Sha1Hash::from_hex(&seed_tree_hash)
            .context("Invalid seed tree hash")?;
        return store.read_tree(hash)
            .context("Failed to read seed tree");
    }

    // No seed tree - return empty tree as fallback
    // This will cause full tree diff (all files are "added")
    Ok(Tree::new())
}

/// Compute the set of paths that differ between two trees
///
/// Used when publishing with seed as base - computes all paths that
/// changed from seed to current checkpoint.
fn compute_tree_diff_paths(old_tree: &Tree, new_tree: &Tree) -> Vec<PathBuf> {
    use tl_core::TreeDiff;

    let diff = TreeDiff::diff(old_tree, new_tree);

    let mut paths = Vec::with_capacity(
        diff.added.len() + diff.removed.len() + diff.modified.len()
    );

    // Convert SmallVec<[u8; 64]> paths to PathBuf
    for (path_bytes, _entry) in diff.added {
        if let Ok(path_str) = std::str::from_utf8(&path_bytes) {
            paths.push(PathBuf::from(path_str));
        }
    }

    for (path_bytes, _entry) in diff.removed {
        if let Ok(path_str) = std::str::from_utf8(&path_bytes) {
            paths.push(PathBuf::from(path_str));
        }
    }

    for (path_bytes, _old_entry, _new_entry) in diff.modified {
        if let Ok(path_str) = std::str::from_utf8(&path_bytes) {
            paths.push(PathBuf::from(path_str));
        }
    }

    paths
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
