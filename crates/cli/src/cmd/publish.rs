//! Publish checkpoint(s) to JJ

use anyhow::{anyhow, Context, Result};
use crate::util;
use owo_colors::OwoColorize;
use tl_core::Store;
use journal::{Journal, PinManager};
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

    // 3. Open components
    let store = Store::open(&repo_root)?;
    let journal = Journal::open(&tl_dir.join("journal"))?;
    let pin_manager = PinManager::new(&tl_dir);
    let mapping = JjMapping::open(&tl_dir)?;

    // 4. Parse checkpoint reference (support ranges like HEAD~10..HEAD or HEAD~10)
    let checkpoints = if checkpoint_ref.contains("..") {
        // Range syntax (e.g., HEAD~10..HEAD)
        parse_checkpoint_range(checkpoint_ref, &journal, &pin_manager)?
    } else if checkpoint_ref.contains('~') && !checkpoint_ref.ends_with('~') {
        // HEAD~N syntax means "from HEAD~N to HEAD"
        let range = format!("{}..HEAD", checkpoint_ref);
        parse_checkpoint_range(&range, &journal, &pin_manager)?
    } else {
        // Single checkpoint
        let checkpoint_id = util::resolve_checkpoint_ref(
            checkpoint_ref, &journal, &pin_manager
        )?;
        let cp = journal.get(&checkpoint_id)?
            .ok_or_else(|| anyhow!("Checkpoint not found: {}", checkpoint_ref))?;
        vec![cp]
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

    // 8. Create bookmark if specified (using native jj-lib API)
    if let Some(bookmark_name) = bookmark {
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

        println!("{} Created bookmark: {}", "✓".green(), bookmark_display.yellow());
    }

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

fn parse_checkpoint_range(
    range: &str,
    journal: &Journal,
    pin_manager: &PinManager,
) -> Result<Vec<journal::Checkpoint>> {
    let parts: Vec<&str> = range.split("..").collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid range syntax. Use: <start>..<end>");
    }

    let start_id = util::resolve_checkpoint_ref(parts[0], journal, pin_manager)?;
    let end_id = util::resolve_checkpoint_ref(parts[1], journal, pin_manager)?;

    // Walk backwards from end until we hit start
    let mut checkpoints = Vec::new();
    let mut current_id = end_id;

    loop {
        let cp = journal.get(&current_id)?
            .ok_or_else(|| anyhow!("Checkpoint not found in range"))?;

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
