//! Push to Git remote via JJ (Native Implementation)
//!
//! Features:
//! - Pre-validates branches before push
//! - Reports per-branch results
//! - Clear error messages for diverged branches

use crate::util;
use anyhow::{anyhow, Context, Result};
use jj::git_ops::BranchPushStatus;
use owo_colors::OwoColorize;

pub async fn run(bookmark: Option<String>, all: bool, force: bool) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'jj git init' first.");
    }

    // 3. Load JJ workspace and push using native API
    let (remote_name, default_branch) = util::resolve_git_sync_target(&repo_root)?;
    println!("{}", format!("Pushing to {}...", remote_name).dimmed());
    let mut workspace = jj::load_workspace(&repo_root).context("Failed to load JJ workspace")?;

    let bookmark_was_explicit = bookmark.is_some();
    let resolved_bookmark = if all {
        None
    } else {
        Some(match bookmark {
            Some(bookmark) => bookmark,
            None => default_branch,
        })
    };
    let bookmark_ref = resolved_bookmark.as_deref();

    // Fast path: when we're pushing the current Git branch tip as-is, the native
    // git CLI is both correct and much faster than loading JJ push machinery.
    if !all {
        if let Some(branch_name) = bookmark_ref {
            if util::git_current_branch(&repo_root)?.as_deref() == Some(branch_name) {
                let git_head = util::git_resolve_ref(&repo_root, "HEAD")?;
                let workspace =
                    jj::load_workspace(&repo_root).context("Failed to load JJ workspace")?;
                let bookmark_head = jj::git_ops::get_local_bookmark_head(&workspace, branch_name)?;

                if git_head.is_some() && git_head == bookmark_head {
                    if let Err(err) = util::git_push_head(&repo_root, &remote_name, branch_name, force) {
                        let err_text = err.to_string();
                        if !force
                            && (err_text.contains("failed to push some refs")
                                || err_text.contains("fetch first")
                                || err_text.contains("non-fast-forward"))
                        {
                            return Err(anyhow!(
                                "Push aborted for one or more bookmarks. Use 'tl pull' to sync or '--force' to overwrite the remote."
                            ));
                        }
                        return Err(err);
                    }

                    println!(
                        "{} Pushed 1 bookmark(s) to remote",
                        "✓".green(),
                    );
                    println!(
                        "  {} {}",
                        branch_name.cyan(),
                        "(git fast path)".dimmed()
                    );
                    return Ok(());
                }
            }
        }
    }

    // Execute native git push (now returns detailed results)
    let results =
        jj::git_ops::native_git_push(&mut workspace, &remote_name, bookmark_ref, all, force)?;

    // Show auto-detected bookmark if neither --all nor -b was specified
    if !bookmark_was_explicit && !all && !results.is_empty() {
        if let Some(first_result) = results.first() {
            println!(
                "{}",
                format!("Using bookmark: {}", first_result.name).dimmed()
            );
        }
    }

    // 4. Display results
    let pushed_count = results
        .iter()
        .filter(|r| r.status == BranchPushStatus::Pushed)
        .count();
    let up_to_date_count = results
        .iter()
        .filter(|r| r.status == BranchPushStatus::UpToDate)
        .count();

    if pushed_count == 0 && up_to_date_count > 0 {
        println!("{} Already up to date", "✓".green());
    } else if pushed_count > 0 {
        println!(
            "{} Pushed {} bookmark(s) to remote",
            "✓".green(),
            pushed_count.to_string().green()
        );
    }

    // Show per-branch details
    for result in &results {
        match &result.status {
            BranchPushStatus::Pushed => {
                let old = result
                    .old_commit
                    .as_ref()
                    .map(|s| &s[..12.min(s.len())])
                    .unwrap_or("(new)");
                let new = result
                    .new_commit
                    .as_ref()
                    .map(|s| &s[..12.min(s.len())])
                    .unwrap_or("???");
                println!(
                    "  {} {} → {}",
                    result.name.cyan(),
                    old.dimmed(),
                    new.green()
                );
            }
            BranchPushStatus::UpToDate => {
                println!("  {} {}", result.name.cyan(), "(up to date)".dimmed());
            }
            BranchPushStatus::Diverged => {
                println!(
                    "  {} {}",
                    result.name.cyan(),
                    "DIVERGED (use --force)".red()
                );
            }
            BranchPushStatus::Rejected(reason) => {
                println!("  {} {} {}", result.name.cyan(), "REJECTED:".red(), reason);
            }
            BranchPushStatus::Skipped => {
                println!("  {} {}", result.name.cyan(), "(skipped)".dimmed());
            }
        }
    }

    let had_failures = results.iter().any(|result| {
        matches!(
            result.status,
            BranchPushStatus::Diverged | BranchPushStatus::Rejected(_)
        )
    });
    if had_failures {
        return Err(anyhow!(
            "Push aborted for one or more bookmarks. Use 'tl pull' to sync or '--force' to overwrite the remote."
        ));
    }

    Ok(())
}
