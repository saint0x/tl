//! Publish checkpoint(s) to JJ

use anyhow::{anyhow, Context, Result};
use crate::util;
use owo_colors::OwoColorize;
use tl_core::Store;
use journal::PinManager;
use jj::{JjMapping, publish};
use jj::materialize::{CommitMessageOptions, PublishOptions};

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

    // 6. Configure publish options
    let mut msg_options = CommitMessageOptions::default();
    if let Some(template) = message_template {
        msg_options.template = Some(template);
    }

    let publish_options = PublishOptions {
        auto_pin: if no_pin { None } else { Some("published".to_string()) },
        message_options: msg_options,
        compact_range: compact,
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

    // 8. Create bookmark (auto-create snap/HEAD if not specified)
    let bookmark_name = bookmark.unwrap_or_else(|| "HEAD".to_string());
    let last_commit_id = commit_ids.last().unwrap();

    // Load workspace and create bookmark natively
    let mut workspace = jj::load_workspace(&repo_root)?;
    jj::create_bookmark_native(&mut workspace, &bookmark_name, last_commit_id)
        .context("Failed to create JJ bookmark")?;

    let bookmark_display = if bookmark_name.starts_with("snap/") {
        bookmark_name.clone()
    } else {
        format!("snap/{}", bookmark_name)
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
