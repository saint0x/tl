//! Show diff between checkpoints

use crate::util;
use anyhow::{anyhow, Context, Result};
use core::{Store, TreeDiff};
use journal::{Journal, PinManager};
use owo_colors::OwoColorize;
use std::path::Path;

pub async fn run(checkpoint_a: &str, checkpoint_b: &str) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Open journal, store, and pin manager
    let journal_path = tl_dir.join("journal");
    let journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    let store = Store::open(&repo_root)?;

    let pin_manager = PinManager::new(&tl_dir);

    // 3. Resolve checkpoint references
    let id_a = util::resolve_checkpoint_ref(checkpoint_a, &journal, &pin_manager)?;
    let id_b = util::resolve_checkpoint_ref(checkpoint_b, &journal, &pin_manager)?;

    // 4. Get checkpoints
    let cp_a = journal.get(&id_a)?
        .ok_or_else(|| anyhow!("Checkpoint {} not found", id_a))?;
    let cp_b = journal.get(&id_b)?
        .ok_or_else(|| anyhow!("Checkpoint {} not found", id_b))?;

    // 5. Load trees
    let tree_a = store.read_tree(cp_a.root_tree)?;
    let tree_b = store.read_tree(cp_b.root_tree)?;

    // 6. Compute diff
    let diff = TreeDiff::diff(&tree_a, &tree_b);

    // 7. Display diff
    println!("{}", "Diff Summary".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    let id_a_short = id_a.to_string()[..8].to_string();
    let id_b_short = id_b.to_string()[..8].to_string();
    println!("From: {} {}", id_a_short.yellow(), util::format_relative_time(cp_a.ts_unix_ms).dimmed());
    println!("To:   {} {}", id_b_short.yellow(), util::format_relative_time(cp_b.ts_unix_ms).dimmed());
    println!();

    let total_changes = diff.added.len() + diff.removed.len() + diff.modified.len();
    if total_changes == 0 {
        println!("{}", "No changes between checkpoints".dimmed());
        return Ok(());
    }

    // Display added files
    if !diff.added.is_empty() {
        println!("{} Added ({} files)", "A".green().bold(), diff.added.len());
        for (path, _entry) in &diff.added {
            let path_str = Path::new(std::str::from_utf8(path).unwrap_or("<invalid utf8>"));
            println!("  {} {}", "+".green(), path_str.display());
        }
        println!();
    }

    // Display removed files
    if !diff.removed.is_empty() {
        println!("{} Removed ({} files)", "D".red().bold(), diff.removed.len());
        for (path, _entry) in &diff.removed {
            let path_str = Path::new(std::str::from_utf8(path).unwrap_or("<invalid utf8>"));
            println!("  {} {}", "-".red(), path_str.display());
        }
        println!();
    }

    // Display modified files
    if !diff.modified.is_empty() {
        println!("{} Modified ({} files)", "M".yellow().bold(), diff.modified.len());
        for (path, _old_entry, _new_entry) in &diff.modified {
            let path_str = Path::new(std::str::from_utf8(path).unwrap_or("<invalid utf8>"));
            println!("  {} {}", "~".yellow(), path_str.display());
        }
        println!();
    }

    // Summary
    println!(
        "{}",
        format!(
            "Total: {} added, {} removed, {} modified",
            diff.added.len().to_string().green(),
            diff.removed.len().to_string().red(),
            diff.modified.len().to_string().yellow()
        ).dimmed()
    );

    Ok(())
}
