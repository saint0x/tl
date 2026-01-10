//! Git tag management
//!
//! Provides Git-compatible tag operations through JJ's bookmark system.
//! Tags are implemented as immutable bookmarks in JJ that are also synced
//! to Git refs/tags/.

use crate::data_access;
use crate::util;
use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::process::Command;

/// List all tags
pub async fn run_list() -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // Use git to list tags (most reliable method)
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("tag")
        .arg("-l")
        .arg("--sort=-creatordate")  // Sort by date, newest first
        .output()
        .context("Failed to execute git tag command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git tag failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        println!("{}", "No tags found".dimmed());
        return Ok(());
    }

    println!("{}", "Tags:".bold());
    for line in stdout.lines() {
        println!("  {}", line.cyan());
    }

    Ok(())
}

/// Create a new tag
pub async fn run_create(
    tag_name: &str,
    checkpoint_ref: Option<String>,
    message: Option<String>,
    force: bool,
) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // Resolve checkpoint to JJ commit if specified
    let target_ref = if let Some(cp_ref) = checkpoint_ref {
        // Resolve checkpoint to commit ID
        let tl_dir = repo_root.join(".tl");
        let resolved = data_access::resolve_checkpoint_refs(&[cp_ref.clone()], &tl_dir).await?;
        let checkpoint_id = resolved[0]
            .ok_or_else(|| anyhow::anyhow!("Checkpoint not found: {}", cp_ref))?;

        // Get JJ commit ID for this checkpoint
        let mapping = jj::JjMapping::open(&tl_dir)?;
        let commit_id = mapping.get_jj_commit(checkpoint_id)?
            .ok_or_else(|| anyhow::anyhow!(
                "Checkpoint {} has not been published to JJ. Run 'tl publish {}' first.",
                cp_ref, cp_ref
            ))?;

        commit_id
    } else {
        // Use current HEAD
        "HEAD".to_string()
    };

    // Validate tag name
    if tag_name.is_empty() {
        anyhow::bail!("Tag name cannot be empty");
    }

    // Check if tag already exists
    if !force {
        let check_output = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .arg("rev-parse")
            .arg(format!("refs/tags/{}", tag_name))
            .output()?;

        if check_output.status.success() {
            anyhow::bail!(
                "Tag '{}' already exists. Use --force to overwrite.",
                tag_name
            );
        }
    }

    // Create the tag using git
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("tag");

    if force {
        cmd.arg("--force");
    }

    let is_annotated = message.is_some();

    if let Some(msg) = message {
        cmd.arg("-a")  // Annotated tag
            .arg(tag_name)
            .arg("-m")
            .arg(msg);
    } else {
        cmd.arg(tag_name);  // Lightweight tag
    }

    cmd.arg(&target_ref);

    let output = cmd.output()
        .context("Failed to execute git tag command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git tag failed: {}", stderr);
    }

    if is_annotated {
        println!("{} Created annotated tag: {}", "✓".green(), tag_name.cyan());
    } else {
        println!("{} Created tag: {}", "✓".green(), tag_name.cyan());
    }

    if target_ref != "HEAD" {
        println!("  {}: {}", "Target".dimmed(), target_ref.dimmed());
    }

    Ok(())
}

/// Delete a tag
pub async fn run_delete(tag_name: &str) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // Delete the tag using git
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("tag")
        .arg("-d")
        .arg(tag_name)
        .output()
        .context("Failed to execute git tag command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git tag failed: {}", stderr);
    }

    println!("{} Deleted tag: {}", "✓".green(), tag_name.cyan());

    Ok(())
}

/// Show tag details
pub async fn run_show(tag_name: &str) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // Show tag details using git
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("show")
        .arg(tag_name)
        .arg("--no-patch")  // Don't show diff
        .output()
        .context("Failed to execute git show command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Tag not found: {}", tag_name);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{}", stdout);

    Ok(())
}

/// Push tags to remote
pub async fn run_push(tag_name: Option<String>, all: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("push")
        .arg("origin");

    if all {
        cmd.arg("--tags");
    } else if let Some(tag) = tag_name {
        cmd.arg(format!("refs/tags/{}", tag));
    } else {
        anyhow::bail!("Must specify --all or provide a tag name");
    }

    println!("{}", "Pushing tags to remote...".dimmed());

    let output = cmd.output()
        .context("Failed to execute git push command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git push failed: {}", stderr);
    }

    if all {
        println!("{} Pushed all tags to origin", "✓".green());
    } else {
        println!("{} Pushed tag to origin", "✓".green());
    }

    Ok(())
}
