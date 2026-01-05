//! Run garbage collection

use crate::locks::GcLock;
use crate::util;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tl_core::Store;
use journal::{GarbageCollector, Journal, PinManager, RetentionPolicy};
use owo_colors::OwoColorize;
use ulid::Ulid;
use std::time::Duration;

pub async fn run() -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Acquire GC lock to prevent concurrent GC or other operations
    // CRITICAL: This lock must be held throughout GC to prevent data races
    let _gc_lock = GcLock::acquire(&tl_dir)
        .context("Failed to acquire GC lock - is another GC or restore in progress?")?;

    // 3. CRITICAL: Stop daemon before GC to avoid journal corruption
    println!("{}", "Stopping daemon for exclusive journal access...".dimmed());

    let socket_path = tl_dir.join("state/daemon.sock");
    let daemon_was_running = if socket_path.exists() {
        match crate::ipc::IpcClient::connect(&socket_path).await {
            Ok(mut client) => {
                client.shutdown().await.ok(); // Send shutdown signal
                // Wait for daemon to stop
                tokio::time::sleep(Duration::from_millis(500)).await;
                true
            }
            Err(_) => false, // Daemon not running
        }
    } else {
        false
    };

    // 3. Now safe to open journal with write access
    let journal_path = tl_dir.join("journal");
    let mut journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    let mut store = Store::open(&repo_root)?;

    // 4. Create pin manager
    let pin_manager = PinManager::new(&tl_dir);

    // 4. Collect workspace checkpoints (if JJ workspace exists)
    let workspace_checkpoints = if jj::detect_jj_workspace(&repo_root)?.is_some() {
        let ws_manager = jj::WorkspaceManager::open(&tl_dir, &repo_root)?;
        let mut checkpoints = HashSet::new();

        for state in ws_manager.list_states()? {
            if let Some(cp_id) = state.current_checkpoint {
                checkpoints.insert(cp_id);
            }
        }

        Some(checkpoints)
    } else {
        None
    };

    // 5. Create GC with default retention policy
    let policy = RetentionPolicy::default();
    let gc = GarbageCollector::new(policy);

    println!("{}", "Running Garbage Collection...".bold());
    println!();

    // 6. Run GC with workspace checkpoint protection
    let metrics = gc.collect(&mut journal, &mut store, &pin_manager, workspace_checkpoints.as_ref())?;

    // 7. Display results
    println!("{}", "GC Complete".green().bold());
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    if metrics.checkpoints_deleted == 0 && metrics.trees_deleted == 0 && metrics.blobs_deleted == 0 {
        println!("{}", "No garbage found - repository is already clean".dimmed());
    } else {
        println!("Checkpoints deleted: {}", metrics.checkpoints_deleted.to_string().yellow());
        println!("Trees deleted:       {}", metrics.trees_deleted.to_string().yellow());
        println!("Blobs deleted:       {}", metrics.blobs_deleted.to_string().yellow());
        println!();
        println!(
            "Space freed:         {}",
            util::format_size(metrics.bytes_freed).green()
        );
    }

    // 8. Drop journal and store to release locks before restarting daemon
    drop(journal);
    drop(store);

    // 9. Restart daemon if it was running before
    if daemon_was_running {
        println!();
        println!("{}", "Restarting daemon...".dimmed());
        crate::daemon::ensure_daemon_running_with_timeout(3).await?;
    }

    Ok(())
}
