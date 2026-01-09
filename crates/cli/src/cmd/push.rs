//! Push to Git remote via JJ (Native Implementation)
//!
//! Features:
//! - Pre-validates branches before push
//! - Reports per-branch results
//! - Clear error messages for diverged branches

use anyhow::{Context, Result};
use crate::util;
use jj::git_ops::{BranchPushResult, BranchPushStatus};
use owo_colors::OwoColorize;

pub async fn run(
    bookmark: Option<String>,
    all: bool,
    force: bool,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Load JJ workspace and push using native API
    println!("{}", "Pushing to Git remote...".dimmed());
    let mut workspace = jj::load_workspace(&repo_root)
        .context("Failed to load JJ workspace")?;

    // Use bookmark name as-is (standard Git naming)
    let bookmark_ref = bookmark.as_deref();

    // Execute native git push (now returns detailed results)
    let results = jj::git_ops::native_git_push(&mut workspace, bookmark_ref, all, force)?;

    // Show auto-detected bookmark if neither --all nor -b was specified
    if bookmark.is_none() && !all && !results.is_empty() {
        if let Some(first_result) = results.first() {
            println!("{}", format!("Using bookmark: {}", first_result.name).dimmed());
        }
    }

    // 4. Display results
    let pushed_count = results.iter()
        .filter(|r| r.status == BranchPushStatus::Pushed)
        .count();
    let up_to_date_count = results.iter()
        .filter(|r| r.status == BranchPushStatus::UpToDate)
        .count();

    if pushed_count == 0 && up_to_date_count > 0 {
        println!("{} Already up to date", "✓".green());
    } else if pushed_count > 0 {
        println!("{} Pushed {} bookmark(s) to remote", "✓".green(), pushed_count.to_string().green());
    }

    // Show per-branch details
    for result in &results {
        match &result.status {
            BranchPushStatus::Pushed => {
                let old = result.old_commit.as_ref()
                    .map(|s| &s[..12.min(s.len())])
                    .unwrap_or("(new)");
                let new = result.new_commit.as_ref()
                    .map(|s| &s[..12.min(s.len())])
                    .unwrap_or("???");
                println!("  {} {} → {}", result.name.cyan(), old.dimmed(), new.green());
            }
            BranchPushStatus::UpToDate => {
                println!("  {} {}", result.name.cyan(), "(up to date)".dimmed());
            }
            BranchPushStatus::Diverged => {
                println!("  {} {}", result.name.cyan(), "DIVERGED (use --force)".red());
            }
            BranchPushStatus::Rejected(reason) => {
                println!("  {} {} {}", result.name.cyan(), "REJECTED:".red(), reason);
            }
            BranchPushStatus::Skipped => {
                println!("  {} {}", result.name.cyan(), "(skipped)".dimmed());
            }
        }
    }

    Ok(())
}
