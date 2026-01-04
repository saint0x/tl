//! Restore working tree to a checkpoint

use crate::util;
use anyhow::{anyhow, Context, Result};
use core::{Entry, Store};
use journal::{Journal, PinManager};
use owo_colors::OwoColorize;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub async fn run(checkpoint: &str) -> Result<()> {
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

    // 3. Resolve checkpoint reference
    let checkpoint_id = util::resolve_checkpoint_ref(checkpoint, &journal, &pin_manager)?;

    // 4. Get checkpoint
    let cp = journal.get(&checkpoint_id)?
        .ok_or_else(|| anyhow!("Checkpoint {} not found", checkpoint_id))?;

    // 5. Load target tree
    let tree = store.read_tree(cp.root_tree)?;

    // 6. Confirm with user
    println!("{}", "Restore Checkpoint".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    let id_short = checkpoint_id.to_string()[..8].to_string();
    println!("Checkpoint: {} {}", id_short.yellow(), util::format_relative_time(cp.ts_unix_ms).dimmed());
    println!("Files:      {}", tree.len());
    println!();
    println!("{}", "⚠️  Warning: This will overwrite your current working directory!".red().bold());
    println!();

    print!("Continue? [y/N] ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if !input.trim().eq_ignore_ascii_case("y") {
        println!("{}", "Restore cancelled".yellow());
        return Ok(());
    }

    println!();
    println!("{}", "Restoring files...".dimmed());

    // 7. Restore files
    let mut restored = 0;
    let mut errors = Vec::new();

    for (path_bytes, entry) in tree.entries_with_paths() {
        let path_str = std::str::from_utf8(path_bytes)
            .context("Invalid UTF-8 in file path")?;
        let file_path = repo_root.join(path_str);

        // Skip if path tries to escape repository or touch protected directories
        if !file_path.starts_with(&repo_root) {
            errors.push(format!("Path escapes repository: {}", path_str));
            continue;
        }

        if path_str.starts_with(".tl/") || path_str.starts_with(".git/") {
            continue; // Skip protected directories
        }

        // Restore file
        match restore_file(&store, &file_path, entry) {
            Ok(()) => {
                restored += 1;
                if restored % 100 == 0 {
                    print!("  Restored {} files...\r", restored);
                    std::io::stdout().flush().ok();
                }
            }
            Err(e) => {
                errors.push(format!("{}: {}", path_str, e));
            }
        }
    }

    println!();
    println!();

    // 8. Display results
    if errors.is_empty() {
        println!("{} Restored {} files successfully", "✓".green(), restored.to_string().green());
    } else {
        println!("{} Restored {} files with {} errors", "⚠".yellow(), restored, errors.len());
        println!();
        println!("{}", "Errors:".red().bold());
        for error in errors.iter().take(10) {
            println!("  {}", error.red());
        }
        if errors.len() > 10 {
            println!("  ... and {} more", errors.len() - 10);
        }
    }

    println!();
    println!("{}", "Note: The daemon will automatically create a new checkpoint reflecting these changes.".dimmed());

    Ok(())
}

/// Restore a single file from a checkpoint
fn restore_file(store: &Store, file_path: &Path, entry: &Entry) -> Result<()> {
    // Create parent directories
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read blob content
    let content = store.blob_store().read_blob(entry.blob_hash)?;

    // Write file
    fs::write(file_path, content)?;

    // Set permissions
    #[cfg(unix)]
    {
        let mode = entry.mode;
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(file_path, permissions)?;
    }

    Ok(())
}
