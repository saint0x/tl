//! Run garbage collection
//!
//! GC retention policy is configurable via system config at ~/.config/tl/config.toml
//!
//! Example configuration:
//! ```toml
//! [gc]
//! retain_count = 2000    # Number of checkpoints to keep
//! retain_hours = 24      # Time window to keep all checkpoints
//! retain_pins = true     # Always keep pinned checkpoints
//! ```

use crate::locks::GcLock;
use crate::system_config;
use crate::util;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tl_core::Store;
use journal::{GarbageCollector, Journal, PinManager};
use owo_colors::OwoColorize;
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

    // 5. Load retention policy from system config
    let system_config = system_config::load()
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to load system config, using defaults: {}", e);
            system_config::SystemConfig::default()
        });
    let policy = system_config.gc.to_retention_policy();
    let gc = GarbageCollector::new(policy.clone());

    // Show retention policy
    println!("Retention policy:");
    println!("  Keep last {} checkpoints", policy.retain_dense_count.to_string().cyan());
    println!("  Keep all within {} hours", (policy.retain_dense_window_ms / 3600000).to_string().cyan());
    println!("  Retain pins: {}", if policy.retain_pins { "yes".green().to_string() } else { "no".red().to_string() });
    println!();

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
