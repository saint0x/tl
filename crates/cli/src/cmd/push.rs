//! Push to Git remote via JJ (Native Implementation)

use anyhow::{Context, Result};
use crate::util;
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

    // Extract bookmark name (remove snap/ prefix if user included it)
    let bookmark_ref = bookmark.as_ref().map(|b| {
        b.strip_prefix("snap/").unwrap_or(b)
    });

    // Execute native git push
    jj::git_ops::native_git_push(&mut workspace, bookmark_ref, all, force)?;

    // 4. Success - display what was pushed
    println!("{} Pushed to remote", "✓".green());

    if let Some(ref b) = bookmark {
        let display_name = if b.starts_with("snap/") {
            b.clone()
        } else {
            format!("snap/{}", b)
        };
        println!("  Bookmark: {}", display_name.cyan());
    } else if all {
        println!("  All bookmarks pushed");
    }

    Ok(())
}
