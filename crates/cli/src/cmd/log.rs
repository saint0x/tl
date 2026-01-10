//! Display checkpoint timeline

use crate::util;
use anyhow::{Context, Result};
use tl_core::{Store, TreeDiff};
use journal::{PinManager, Checkpoint};
use owo_colors::OwoColorize;
use std::collections::HashMap;
use ulid::Ulid;

pub struct LogOptions {
    pub limit: Option<usize>,
    pub oneline: bool,
    pub graph: bool,
    pub author_filter: Option<String>,
    pub grep_filter: Option<String>,
}

pub async fn run(limit: Option<usize>) -> Result<()> {
    run_with_options(LogOptions {
        limit,
        oneline: false,
        graph: false,
        author_filter: None,
        grep_filter: None,
    }).await
}

pub async fn run_with_options(options: LogOptions) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Ensure daemon is running (auto-start with supervisor)
    crate::daemon::ensure_daemon_running().await?;

    // 3. Connect to daemon with retry
    let socket_path = tl_dir.join("state/daemon.sock");
    let resilient_client = crate::ipc::ResilientIpcClient::new(socket_path);
    let mut client = resilient_client.connect_with_retry().await
        .context("Failed to connect to daemon")?;

    // 4. Get checkpoint count and list in one IPC call
    let limit_val = options.limit.unwrap_or(20);
    let (checkpoint_count, mut checkpoints) = client.get_log_data(Some(limit_val), None).await?;

    if checkpoint_count == 0 {
        println!("{}", "No checkpoints yet".dimmed());
        println!();
        println!("{}", "Tip: Daemon is running and tracking changes automatically".dimmed());
        return Ok(());
    }

    // 5. Open store for tree diffs (read-only, safe)
    let store = Store::open(&repo_root)?;

    // 5.5. Load all pins and create reverse mapping (checkpoint → pin names)
    let pin_manager = PinManager::new(&tl_dir);
    let all_pins = pin_manager.list_pins()?;
    let mut pins_by_checkpoint: HashMap<Ulid, Vec<String>> = HashMap::new();
    for (name, id) in all_pins {
        pins_by_checkpoint.entry(id).or_default().push(name);
    }

    // 6. Apply filters
    if let Some(ref grep_pattern) = options.grep_filter {
        checkpoints.retain(|cp| {
            // Search in touched paths
            cp.touched_paths.iter().any(|p| {
                p.to_string_lossy().contains(grep_pattern)
            })
        });
    }

    // Author filtering would require storing author info in checkpoints (future enhancement)

    // 7. Display based on format
    if options.oneline {
        display_oneline(&checkpoints, &pins_by_checkpoint);
    } else if options.graph {
        display_graph(&checkpoints, &pins_by_checkpoint, &store);
    } else {
        display_full(&checkpoints, &pins_by_checkpoint, &store, limit_val, checkpoint_count)?;
    }

    Ok(())
}

/// Display in one-line format (like git log --oneline)
fn display_oneline(checkpoints: &[Checkpoint], pins_by_checkpoint: &HashMap<Ulid, Vec<String>>) {
    for checkpoint in checkpoints {
        let id_short = checkpoint.id.to_string()[..8].to_string();
        let time_str = util::format_relative_time(checkpoint.ts_unix_ms);
        let reason = format!("{:?}", checkpoint.reason);

        print!("{} ", id_short.yellow());

        // Display pin names if any
        if let Some(pin_names) = pins_by_checkpoint.get(&checkpoint.id) {
            let mut sorted_names = pin_names.clone();
            sorted_names.sort();
            for name in sorted_names {
                print!("({}) ", name.cyan());
            }
        }

        print!("{} ", reason.cyan());
        print!("- ");

        // Show first changed file or file count
        if let Some(first_path) = checkpoint.touched_paths.first() {
            print!("{}", first_path.display().to_string().dimmed());
            if checkpoint.touched_paths.len() > 1 {
                print!(" (+{} more)", checkpoint.touched_paths.len() - 1);
            }
        } else {
            print!("no changes");
        }

        print!(" [{}]", time_str.dimmed());

        println!();
    }
}

/// Display with ASCII graph (like git log --graph)
fn display_graph(
    checkpoints: &[Checkpoint],
    pins_by_checkpoint: &HashMap<Ulid, Vec<String>>,
    store: &Store,
) {
    for (idx, checkpoint) in checkpoints.iter().enumerate() {
        let id_short = checkpoint.id.to_string()[..8].to_string();
        let time_str = util::format_relative_time(checkpoint.ts_unix_ms);
        let reason = format!("{:?}", checkpoint.reason);

        // Draw graph line
        if checkpoint.parent.is_some() {
            print!("│ ");
        } else {
            print!("* ");  // Root commit
        }

        print!("{} ", id_short.yellow());

        // Display pin names if any
        if let Some(pin_names) = pins_by_checkpoint.get(&checkpoint.id) {
            let mut sorted_names = pin_names.clone();
            sorted_names.sort();
            for name in sorted_names {
                print!("({}) ", name.cyan());
            }
        }

        println!("{} - {}", reason.cyan(), time_str.dimmed());

        // Show connector to parent
        if idx + 1 < checkpoints.len() {
            println!("│");
        }
    }
}

/// Display in full format (default)
fn display_full(
    checkpoints: &[Checkpoint],
    pins_by_checkpoint: &HashMap<Ulid, Vec<String>>,
    store: &Store,
    limit_val: usize,
    checkpoint_count: usize,
) -> Result<()> {
    println!("{}", "Checkpoint History".bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    for (idx, checkpoint) in checkpoints.iter().enumerate() {
        let id_short = checkpoint.id.to_string()[..8].to_string();
        let time_str = util::format_relative_time(checkpoint.ts_unix_ms);
        let reason = format!("{:?}", checkpoint.reason);

        // Calculate diff summary if there's a previous checkpoint
        let diff_summary = if let Some(prev_id) = checkpoint.parent {
            // Try to find parent in already-fetched checkpoints
            let prev_cp = checkpoints.iter().skip(idx + 1).find(|cp| cp.id == prev_id);

            if let Some(prev_cp) = prev_cp {
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
                print!("{}", "no changes".dimmed());
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
                print!("{}", parts.join(", "));
            }
        } else {
            print!("{}", checkpoint.meta.files_changed);
        }

        // Display pin names if any
        if let Some(pin_names) = pins_by_checkpoint.get(&checkpoint.id) {
            let mut sorted_names = pin_names.clone();
            sorted_names.sort();
            let pins_display = sorted_names.join(", ");
            print!("   📌 {}", pins_display.cyan());
        }

        println!(); // Complete the header line

        // Show changed paths (up to 3)
        if !checkpoint.touched_paths.is_empty() {
            for path in checkpoint.touched_paths.iter().take(3) {
                let path_str = path.display().to_string();

                // Determine status symbol
                let symbol = if let Some((_added, _removed, _modified)) = diff_summary {
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
    if checkpoint_count > limit_val {
        println!(
            "{}",
            format!("Showing {} of {} total checkpoints", limit_val, checkpoint_count).dimmed()
        );
        println!("{}", format!("Use 'tl log --limit {}' to see more", checkpoint_count.min(limit_val * 2)).dimmed());
    } else {
        println!("{}", format!("Total: {} checkpoints", checkpoint_count).dimmed());
    }

    Ok(())
}
