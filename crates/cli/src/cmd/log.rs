//! Display checkpoint timeline

use crate::util;
use anyhow::{Context, Result};
use core::{Store, Tree, TreeDiff};
use journal::Journal;
use owo_colors::OwoColorize;

pub async fn run(limit: Option<usize>) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Open journal and store
    let journal_path = tl_dir.join("journal");
    let journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    let store = Store::open(&repo_root)?;

    // 3. Get checkpoints (limited)
    let checkpoint_count = journal.count();
    if checkpoint_count == 0 {
        println!("{}", "No checkpoints yet".dimmed());
        println!();
        println!("{}", "Tip: Start the daemon with 'tl start' to begin tracking changes".dimmed());
        return Ok(());
    }

    let limit = limit.unwrap_or(20);
    let mut checkpoints = Vec::new();

    // Collect checkpoints in reverse order (newest first)
    let mut current_id = journal.latest()?.map(|cp| cp.id);
    while let Some(id) = current_id {
        if checkpoints.len() >= limit {
            break;
        }

        let checkpoint = journal.get(&id)?
            .context("Checkpoint not found in journal")?;

        current_id = checkpoint.parent;
        checkpoints.push(checkpoint);
    }

    // 4. Display each checkpoint with diff summary
    println!("{}", "Checkpoint History".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    for checkpoint in &checkpoints {
        let id_short = checkpoint.id.to_string()[..8].to_string();
        let time_str = util::format_relative_time(checkpoint.ts_unix_ms);
        let reason = format!("{:?}", checkpoint.reason);

        // Calculate diff summary if there's a previous checkpoint
        let diff_summary = if let Some(prev_id) = checkpoint.parent {
            if let Ok(Some(prev_cp)) = journal.get(&prev_id) {
                match (
                    store.read_tree(prev_cp.root_tree),
                    store.read_tree(checkpoint.root_tree)
                ) {
                    (Ok(old_tree), Ok(new_tree)) => {
                        let diff = TreeDiff::diff(&old_tree, &new_tree);
                        Some((
                            diff.added.len(),
                            diff.removed.len(),
                            diff.modified.len()
                        ))
                    }
                    _ => None
                }
            } else {
                None
            }
        } else {
            // First checkpoint - count all entries as added
            if let Ok(tree) = store.read_tree(checkpoint.root_tree) {
                Some((tree.len(), 0, 0))
            } else {
                None
            }
        };

        // Format the header line
        print!("{} ", id_short.yellow());
        print!("{:15} ", time_str);
        print!("[{}] ", reason.cyan());

        if let Some((added, removed, modified)) = diff_summary {
            let total = added + removed + modified;
            if total == 0 {
                println!("{}", "no changes".dimmed());
            } else {
                let mut parts = Vec::new();
                if modified > 0 {
                    parts.push(format!("{} modified", modified));
                }
                if added > 0 {
                    parts.push(format!("{} added", added));
                }
                if removed > 0 {
                    parts.push(format!("{} removed", removed));
                }
                println!("{}", parts.join(", "));
            }
        } else {
            println!("{}", checkpoint.meta.files_changed);
        }

        // Show changed paths (up to 3)
        if !checkpoint.touched_paths.is_empty() {
            for path in checkpoint.touched_paths.iter().take(3) {
                let path_str = path.display().to_string();

                // Determine status symbol
                let symbol = if let Some((added, removed, modified)) = diff_summary {
                    // Try to determine if this specific path was A/M/D
                    // For simplicity, just use M for now
                    "M"
                } else {
                    "M"
                };

                println!("  {} {}", symbol.yellow(), path_str);
            }

            if checkpoint.touched_paths.len() > 3 {
                println!("  {}", format!("... and {} more", checkpoint.touched_paths.len() - 3).dimmed());
            }
        }

        println!();
    }

    // Summary
    if checkpoint_count > limit {
        println!(
            "{}",
            format!("Showing {} of {} total checkpoints", limit, checkpoint_count).dimmed()
        );
        println!("{}", format!("Use 'tl log --limit {}' to see more", checkpoint_count.min(limit * 2)).dimmed());
    } else {
        println!("{}", format!("Total: {} checkpoints", checkpoint_count).dimmed());
    }

    Ok(())
}
