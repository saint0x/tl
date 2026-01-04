//! Pin a checkpoint with a name

use crate::util;
use anyhow::{anyhow, Context, Result};
use journal::{Journal, PinManager};
use owo_colors::OwoColorize;

pub async fn run(checkpoint: &str, name: &str) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Open journal and pin manager
    let journal_path = tl_dir.join("journal");
    let journal = Journal::open(&journal_path)
        .context("Failed to open checkpoint journal")?;

    let pin_manager = PinManager::new(&tl_dir);

    // 3. Resolve checkpoint reference
    let checkpoint_id = util::resolve_checkpoint_ref(checkpoint, &journal, &pin_manager)?;

    // 4. Verify checkpoint exists
    journal.get(&checkpoint_id)?
        .ok_or_else(|| anyhow!("Checkpoint {} not found", checkpoint_id))?;

    // 5. Pin the checkpoint
    pin_manager.pin(name, checkpoint_id)?;

    let id_short = checkpoint_id.to_string()[..8].to_string();
    println!("{} Created pin '{}' → {}", "✓".green(), name.yellow(), id_short.cyan());

    Ok(())
}
