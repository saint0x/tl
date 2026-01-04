//! Remove a pin

use crate::util;
use anyhow::{Context, Result};
use journal::PinManager;
use owo_colors::OwoColorize;

pub async fn run(name: &str) -> Result<()> {
    // 1. Find repository root
    let repo_root = util::find_repo_root()
        .context("Failed to find repository")?;

    let tl_dir = repo_root.join(".tl");

    // 2. Create pin manager
    let pin_manager = PinManager::new(&tl_dir);

    // 3. Get checkpoint ID before removing (for display)
    let pins = pin_manager.list_pins()?;
    let checkpoint_id = pins.iter()
        .find(|(pin_name, _)| pin_name == name)
        .map(|(_, id)| *id)
        .ok_or_else(|| anyhow::anyhow!("Pin '{}' not found", name))?;

    // 4. Remove the pin
    pin_manager.unpin(name)?;

    let id_short = checkpoint_id.to_string()[..8].to_string();
    println!("{} Removed pin '{}' (was {})", "✓".green(), name.yellow(), id_short.cyan());

    Ok(())
}
