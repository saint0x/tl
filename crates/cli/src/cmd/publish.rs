//! Publish checkpoint(s) to JJ

use anyhow::{anyhow, Context, Result};
use crate::util;
use owo_colors::OwoColorize;
use std::collections::HashSet;
use tl_core::Store;
use journal::{Checkpoint, PinManager};
use jj::{JjMapping, publish};
use jj::materialize::{CommitMessageOptions, PublishOptions};
use ulid::Ulid;

pub async fn run(
    checkpoint_ref: &str,
    bookmark: Option<String>,
    compact: bool,
    no_pin: bool,
    message_template: Option<String>,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Ensure daemon running (auto-starts if needed)
    crate::daemon::ensure_daemon_running().await?;

    // 4. Open components
    let store = Store::open(&repo_root)?;
    let pin_manager = PinManager::new(&tl_dir);
    let mapping = JjMapping::open(&tl_dir)?;

    // 5. Parse checkpoint reference (support ranges like HEAD~10..HEAD or HEAD~10)
    let checkpoints = if checkpoint_ref.contains("..") {
        // Range syntax (e.g., HEAD~10..HEAD)
        parse_checkpoint_range(checkpoint_ref, &tl_dir).await?
    } else if checkpoint_ref.contains('~') && !checkpoint_ref.ends_with('~') {
        // HEAD~N syntax means "from HEAD~N to HEAD"
        let range = format!("{}..HEAD", checkpoint_ref);
        parse_checkpoint_range(&range, &tl_dir).await?
    } else {
        // Single checkpoint
        let ids = crate::data_access::resolve_checkpoint_refs(&[checkpoint_ref.to_string()], &tl_dir).await?;
        let checkpoint_id = ids[0].ok_or_else(||
            anyhow!("Checkpoint '{}' not found or ambiguous", checkpoint_ref))?;

        let checkpoints = crate::data_access::get_checkpoints(&[checkpoint_id], &tl_dir).await?;
        let cp = checkpoints[0].as_ref().ok_or_else(||
            anyhow!("Checkpoint not found: {}", checkpoint_ref))?;
        vec![cp.clone()]
    };

    // 5. Validate checkpoints not already published (unless compact mode)
    if !compact {
        let mut already_published = Vec::new();
        for cp in &checkpoints {
            if let Some(jj_commit_id) = mapping.get_jj_commit(cp.id)? {
                already_published.push((cp.id, jj_commit_id));
            }
        }

        if !already_published.is_empty() {
            println!("{} The following checkpoints are already published:", "Warning:".yellow());
            for (cp_id, commit_id) in &already_published {
                let short_cp = &cp_id.to_string()[..8];
                let short_commit = &commit_id[..12.min(commit_id.len())];
                println!("  {} → {}", short_cp.yellow(), short_commit.cyan());
            }
            println!();
            println!("{}", "Use --compact to squash into a single commit, or select unpublished checkpoints.".dimmed());
            anyhow::bail!("Some checkpoints already published");
        }
    }

    // 5.5 Smart chain publishing: find nearest published ancestor
    // If ancestor is within MAX_GAP checkpoints, publish the minimal chain
    // Otherwise, use tree-diff fallback for efficiency
    let checkpoints = smart_chain_publish(checkpoints, &mapping, &tl_dir).await?;

    // 5.6 Compute accumulated_paths for the first checkpoint if its parent isn't published
    // This avoids O(all_files) tree diff by using O(chain_length * touched_paths)
    let accumulated_paths = if let Some(first_cp) = checkpoints.first() {
        // Check if first checkpoint's parent is published
        let parent_published = match first_cp.parent {
            Some(parent_id) => mapping.get_jj_commit(parent_id)?.is_some(),
            None => true, // Root checkpoint - seed commit is the parent
        };

        if !parent_published {
            // Compute accumulated touched_paths from chain
            Some(accumulate_touched_paths(first_cp, &mapping, &tl_dir).await?)
        } else {
            None
        }
    } else {
        None
    };

    // 6. Configure publish options
    let mut msg_options = CommitMessageOptions::default();
    if let Some(template) = message_template {
        msg_options.template = Some(template);
    }

    let publish_options = PublishOptions {
        auto_pin: if no_pin { None } else { Some("published".to_string()) },
        message_options: msg_options,
        compact_range: compact,
        accumulated_paths,
    };

    // 7. Publish checkpoint(s)
    if checkpoints.len() >= 6 {
        println!("{}", format!("Publishing {} checkpoints to JJ...", checkpoints.len()).dimmed());
    } else {
        println!("{}", "Publishing checkpoints to JJ...".dimmed());
    }

    let commit_ids = publish::publish_range(
        checkpoints.clone(),
        &store,
        &repo_root,
        &mapping,
        &publish_options,
    )?;

    // CRITICAL: Flush mapping to disk immediately after publish
    // Without this, a crash could lose the checkpoint→JJ commit mappings,
    // causing duplicate JJ commits on retry
    mapping.flush()
        .context("Failed to flush checkpoint mapping to disk")?;

    // 8. Create bookmark (auto-create tl/HEAD if not specified)
    let bookmark_name = bookmark.unwrap_or_else(|| "HEAD".to_string());
    let last_commit_id = commit_ids.last().unwrap();

    // Load workspace and create bookmark natively
    let mut workspace = jj::load_workspace(&repo_root)?;
    jj::create_bookmark_native(&mut workspace, &bookmark_name, last_commit_id)
        .context("Failed to create JJ bookmark")?;

    let bookmark_display = if bookmark_name.starts_with("tl/") {
        bookmark_name.clone()
    } else {
        format!("tl/{}", bookmark_name)
    };

    println!("{} Updated bookmark: {}", "✓".green(), bookmark_display.yellow());

    // 9. Auto-pin if configured
    if !no_pin {
        for checkpoint in &checkpoints {
            pin_manager.pin("published", checkpoint.id)?;
        }
    }

    // 10. Display results
    println!();
    println!("{} Published {} checkpoint(s)",
        "✓".green(),
        commit_ids.len().to_string().green()
    );

    for (i, commit_id) in commit_ids.iter().enumerate() {
        let short_id = &checkpoints[i].id.to_string()[..8];
        let short_commit = &commit_id[..12.min(commit_id.len())];
        println!("  {} → {}",
            short_id.yellow(),
            short_commit.cyan()
        );
    }

    Ok(())
}

async fn parse_checkpoint_range(
    range: &str,
    tl_dir: &std::path::Path,
) -> Result<Vec<journal::Checkpoint>> {
    let parts: Vec<&str> = range.split("..").collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid range syntax. Use: <start>..<end>");
    }

    // Resolve both start and end checkpoint references
    let refs = vec![parts[0].to_string(), parts[1].to_string()];
    let ids = crate::data_access::resolve_checkpoint_refs(&refs, tl_dir).await?;

    let start_id = ids[0].ok_or_else(||
        anyhow!("Start checkpoint '{}' not found or ambiguous", parts[0]))?;
    let end_id = ids[1].ok_or_else(||
        anyhow!("End checkpoint '{}' not found or ambiguous", parts[1]))?;

    // Walk backwards from end until we hit start
    let mut checkpoints = Vec::new();
    let mut current_id = end_id;

    loop {
        // Get current checkpoint
        let current_checkpoints = crate::data_access::get_checkpoints(&[current_id], tl_dir).await?;
        let cp = current_checkpoints[0].as_ref().ok_or_else(||
            anyhow!("Checkpoint not found in range"))?;

        checkpoints.push(cp.clone());

        if current_id == start_id {
            break;
        }

        current_id = cp.parent
            .ok_or_else(|| anyhow!("Range includes root checkpoint"))?;
    }

    checkpoints.reverse(); // Oldest first
    Ok(checkpoints)
}

/// Maximum gap between checkpoints before falling back to accumulated paths
/// This limit determines how many checkpoints we'll publish in chain.
/// Beyond this, we use accumulated_paths (still fast) instead of chain publishing.
const MAX_CHAIN_GAP: usize = 100;

/// Smart chain publishing: find nearest published ancestor and publish minimal chain
///
/// Algorithm:
/// 1. For each checkpoint, find nearest published ancestor (NPA)
/// 2. If NPA within MAX_CHAIN_GAP: publish minimal chain from NPA to checkpoint
/// 3. If NPA too distant or not found: mark checkpoint for tree-diff fallback
///
/// This avoids publishing entire history (slow) while ensuring correct incremental conversion.
async fn smart_chain_publish(
    requested: Vec<Checkpoint>,
    mapping: &JjMapping,
    tl_dir: &std::path::Path,
) -> Result<Vec<Checkpoint>> {
    // Track what we already have in the result set
    let mut in_result: HashSet<Ulid> = requested.iter().map(|cp| cp.id).collect();
    let mut ancestors_to_prepend: Vec<Checkpoint> = Vec::new();

    // For each checkpoint, find nearest published ancestor
    for checkpoint in &requested {
        // Skip if this checkpoint is already published
        if mapping.get_jj_commit(checkpoint.id)?.is_some() {
            continue;
        }

        // Find nearest published ancestor within MAX_CHAIN_GAP
        let (chain, _npa_found) = find_chain_to_published_ancestor(
            checkpoint,
            mapping,
            tl_dir,
            MAX_CHAIN_GAP,
            &in_result,
        ).await?;

        // Add ancestors to prepend list (avoiding duplicates)
        for ancestor in chain {
            if !in_result.contains(&ancestor.id) {
                in_result.insert(ancestor.id);
                ancestors_to_prepend.push(ancestor);
            }
        }
    }

    // Sort ancestors by walking from roots to leaves (oldest first)
    // This ensures publishing happens in topological order
    ancestors_to_prepend.sort_by_key(|cp| cp.id); // ULID is time-ordered

    // Report what we're doing
    let ancestor_count = ancestors_to_prepend.len();
    if ancestor_count > 0 {
        println!("{} Including {} ancestor(s) for fast incremental publish",
            "→".cyan(),
            ancestor_count.to_string().yellow()
        );
    }

    // Combine: ancestors first, then requested checkpoints
    let mut result = ancestors_to_prepend;
    result.extend(requested);

    Ok(result)
}

/// Find the chain of unpublished checkpoints from target back to nearest published ancestor
///
/// Returns (chain, npa_found):
/// - chain: checkpoints to publish (oldest first), empty if parent is published
/// - npa_found: true if we found a published ancestor within max_depth
async fn find_chain_to_published_ancestor(
    checkpoint: &Checkpoint,
    mapping: &JjMapping,
    tl_dir: &std::path::Path,
    max_depth: usize,
    already_included: &HashSet<Ulid>,
) -> Result<(Vec<Checkpoint>, bool)> {
    let mut chain: Vec<Checkpoint> = Vec::new();
    let mut current = checkpoint.clone();
    let mut depth = 0;

    loop {
        // Check if we've exceeded max depth
        if depth >= max_depth {
            // Too deep - clear chain, will use tree-diff fallback
            return Ok((Vec::new(), false));
        }

        // Get parent
        let parent_id = match current.parent {
            Some(id) => id,
            None => {
                // Root checkpoint - use seed as base (handled in materialize.rs)
                // Return collected chain so far
                chain.reverse();
                return Ok((chain, true));
            }
        };

        // Check if parent is already in our publish set
        if already_included.contains(&parent_id) {
            // Parent will be published - we're good
            chain.reverse();
            return Ok((chain, true));
        }

        // Check if parent is already published to JJ
        if mapping.get_jj_commit(parent_id)?.is_some() {
            // Found published ancestor - return chain
            chain.reverse();
            return Ok((chain, true));
        }

        // Parent not published - add to chain and continue walking back
        let parent_checkpoints = crate::data_access::get_checkpoints(&[parent_id], tl_dir).await?;
        let parent = match &parent_checkpoints[0] {
            Some(cp) => cp.clone(),
            None => {
                // Parent not found (possibly GC'd) - use tree-diff fallback
                return Ok((Vec::new(), false));
            }
        };

        chain.push(parent.clone());
        current = parent;
        depth += 1;
    }
}

/// Compute accumulated touched_paths from checkpoint back to nearest published ancestor
///
/// Returns the union of all touched_paths from the checkpoint chain back to
/// the nearest published ancestor. This is O(chain_length * avg_touched_paths)
/// instead of O(all_files_in_repo) for tree diff.
///
/// Uses IPC to load checkpoints when daemon is running (avoids journal lock conflict).
async fn accumulate_touched_paths(
    checkpoint: &Checkpoint,
    mapping: &JjMapping,
    tl_dir: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>> {
    let mut paths: HashSet<std::path::PathBuf> = HashSet::new();
    let mut current = checkpoint.clone();

    // Walk back through checkpoint chain
    loop {
        // Add touched paths from current checkpoint
        for path in &current.touched_paths {
            paths.insert(path.clone());
        }

        // Check if parent exists and is published
        let parent_id = match current.parent {
            Some(id) => id,
            None => break, // Root checkpoint
        };

        // If parent is published, we're done
        if mapping.get_jj_commit(parent_id)?.is_some() {
            break;
        }

        // Load parent checkpoint via IPC
        let cps = crate::data_access::get_checkpoints(&[parent_id], tl_dir).await?;
        current = match &cps[0] {
            Some(c) => c.clone(),
            None => break, // Parent not found (possibly GC'd)
        };
    }

    Ok(paths.into_iter().collect())
}
