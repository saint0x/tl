//! Branch management command
//!
//! List, create, and delete branches (JJ bookmarks with standard Git naming).

use anyhow::{Context, Result};
use crate::util;
use owo_colors::OwoColorize;

/// Run the branch command
pub async fn run(
    remote: bool,
    all: bool,
    delete: Option<String>,
    create: Option<(String, String)>,
) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()?;

    // Ensure daemon is running
    crate::daemon::ensure_daemon_running().await?;

    // 2. Verify JJ workspace exists
    if jj::detect_jj_workspace(&repo_root)?.is_none() {
        anyhow::bail!("No JJ workspace found. Run 'tl init' first.");
    }

    // 3. Load workspace
    let mut workspace = jj::load_workspace(&repo_root)
        .context("Failed to load JJ workspace")?;

    // 4. Handle delete operation
    if let Some(branch_name) = delete {
        return delete_branch(&mut workspace, &branch_name);
    }

    // 5. Handle create operation
    if let Some((branch_name, checkpoint_ref)) = create {
        return create_branch(&repo_root, &mut workspace, &branch_name, &checkpoint_ref).await;
    }

    // 6. List branches
    list_branches(&workspace, remote, all)?;

    Ok(())
}

/// List branches
fn list_branches(
    workspace: &jj_lib::workspace::Workspace,
    show_remote: bool,
    show_all: bool,
) -> Result<()> {
    let local_branches = jj::git_ops::get_local_branches(workspace)?;

    if local_branches.is_empty() && !show_remote && !show_all {
        println!("{}", "No local branches".dimmed());
        println!();
        println!("{}", "Create one with:".dimmed());
        println!("  tl publish HEAD");
        return Ok(());
    }

    // Show local branches
    if !local_branches.is_empty() {
        println!("{}", "Local branches:".bold());
        for branch in &local_branches {
            let short_id = &branch.commit_id[..12.min(branch.commit_id.len())];

            let status = if let Some(ref remote_id) = branch.remote_commit_id {
                let remote_short = &remote_id[..12.min(remote_id.len())];
                format!("{} (remote: {})", "differs".yellow(), remote_short.dimmed())
            } else if branch.has_remote {
                "✓ synced".green().to_string()
            } else {
                "local only".cyan().to_string()
            };

            println!("  {} {} {}",
                branch.name.cyan(),
                short_id.dimmed(),
                status);
        }
        println!();
    }

    // Show remote branches (if -r or -a flag)
    if show_remote || show_all {
        let remote_branches = jj::git_ops::get_remote_only_branches(workspace)?;
        let remote_updates = jj::git_ops::get_remote_branch_updates(workspace)?;

        // Combine remote-only and all remote tracking branches
        let all_remote: Vec<_> = if show_all {
            remote_updates
        } else {
            remote_branches.into_iter()
                .map(|b| jj::RemoteBranchInfo {
                    name: b.name,
                    remote_commit_id: b.remote_commit_id,
                    local_commit_id: None,
                    is_diverged: false,
                    commits_ahead: 0,
                    commits_behind: 0,
                })
                .collect()
        };

        if !all_remote.is_empty() {
            println!("{}", "Remote branches (origin):".bold());
            for branch in &all_remote {
                let remote_id = branch.remote_commit_id.as_ref()
                    .map(|s| &s[..12.min(s.len())])
                    .unwrap_or("???");

                let status = if branch.local_commit_id.is_some() {
                    if branch.is_diverged {
                        "diverged".red().to_string()
                    } else {
                        "tracked".dimmed().to_string()
                    }
                } else {
                    "not tracked".yellow().to_string()
                };

                println!("  {} {} {}",
                    branch.name.cyan(),
                    remote_id.dimmed(),
                    status);
            }
            println!();
        } else {
            println!("{}", "No remote branches".dimmed());
            println!();
        }
    }

    Ok(())
}

/// Delete a branch
fn delete_branch(workspace: &mut jj_lib::workspace::Workspace, branch_name: &str) -> Result<()> {
    println!("Deleting branch {}...", branch_name.cyan());

    jj::git_ops::delete_local_branch(workspace, branch_name)?;

    println!("{} Deleted branch {}", "✓".green(), branch_name.cyan());
    println!();
    println!("{}", "Note: Remote branch not affected. Use 'git push origin --delete' to delete remote.".dimmed());

    Ok(())
}

/// Create a branch at a checkpoint
async fn create_branch(
    repo_root: &std::path::Path,
    workspace: &mut jj_lib::workspace::Workspace,
    branch_name: &str,
    checkpoint_ref: &str,
) -> Result<()> {
    let tl_dir = repo_root.join(".tl");

    // Resolve checkpoint reference
    let ids = crate::data_access::resolve_checkpoint_refs(&[checkpoint_ref.to_string()], &tl_dir).await?;
    let checkpoint_id = ids[0].ok_or_else(||
        anyhow::anyhow!("Checkpoint '{}' not found", checkpoint_ref))?;

    // Get checkpoint's JJ commit ID
    let jj_mapping = jj::JjMapping::open(&tl_dir)?;
    let checkpoint_id_str = checkpoint_id.to_string();
    let checkpoint_short = &checkpoint_id_str[..8];
    let commit_id_hex = jj_mapping.get_jj_commit(checkpoint_id)?
        .ok_or_else(|| anyhow::anyhow!("Checkpoint {} not published to JJ. Run 'tl publish {}' first.",
            checkpoint_short, checkpoint_ref))?;

    let commit_short = &commit_id_hex[..12.min(commit_id_hex.len())];
    println!("Creating branch {} at {}...", branch_name.cyan(), commit_short.dimmed());

    jj::create_bookmark_native(workspace, branch_name, &commit_id_hex)?;

    println!("{} Created branch {}", "✓".green(), branch_name.cyan());
    println!();
    println!("{}", "Push to remote with: tl push".dimmed());

    Ok(())
}
