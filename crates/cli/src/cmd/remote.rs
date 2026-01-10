//! Git remote management
//!
//! Provides commands to manage Git remotes for push/pull operations.

use crate::util;
use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::process::Command;

/// List all remotes
pub async fn run_list(verbose: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Check if Git repo exists
    if !repo_root.join(".git").exists() {
        anyhow::bail!("No Git repository found. Run 'tl init' first.");
    }

    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("remote");

    if verbose {
        cmd.arg("-v");
    }

    let output = cmd.output()
        .context("Failed to execute git remote command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git remote failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        println!("{}", "No remotes configured".dimmed());
        return Ok(());
    }

    if verbose {
        println!("{}", "Remotes:".bold());
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let name = parts[0];
                let url = parts[1];
                let direction = parts[2];

                let direction_display = if direction.contains("fetch") {
                    "(fetch)".dimmed()
                } else {
                    "(push)".dimmed()
                };

                println!("  {} {} {}", name.cyan(), url.bright_blue(), direction_display);
            }
        }
    } else {
        println!("{}", "Remotes:".bold());
        for line in stdout.lines() {
            println!("  {}", line.cyan());
        }
    }

    Ok(())
}

/// Add a new remote
pub async fn run_add(name: &str, url: &str, fetch: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Check if Git repo exists
    if !repo_root.join(".git").exists() {
        anyhow::bail!("No Git repository found. Run 'tl init' first.");
    }

    // Validate remote name
    if name.is_empty() {
        anyhow::bail!("Remote name cannot be empty");
    }

    if name.contains(' ') || name.contains('/') {
        anyhow::bail!("Invalid remote name: {}", name);
    }

    // Validate URL
    if url.is_empty() {
        anyhow::bail!("Remote URL cannot be empty");
    }

    // Check if remote already exists
    let check_output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("remote")
        .arg("get-url")
        .arg(name)
        .output()?;

    if check_output.status.success() {
        anyhow::bail!(
            "Remote '{}' already exists. Use 'tl remote set-url' to update it.",
            name
        );
    }

    // Add the remote
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("remote")
        .arg("add")
        .arg(name)
        .arg(url)
        .output()
        .context("Failed to execute git remote add command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Git remote add failed: {}", stderr);
    }

    println!("{} Added remote: {} -> {}", "✓".green(), name.cyan(), url.bright_blue());

    // Fetch from the new remote if requested
    if fetch {
        println!("{}", format!("Fetching from {}...", name).dimmed());

        let fetch_output = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .arg("fetch")
            .arg(name)
            .output()
            .context("Failed to execute git fetch command")?;

        if !fetch_output.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_output.stderr);
            println!("{} Fetch failed: {}", "!".yellow(), stderr);
        } else {
            println!("{} Fetched from {}", "✓".green(), name.cyan());
        }
    }

    Ok(())
}

/// Remove a remote
pub async fn run_remove(name: &str) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Check if Git repo exists
    if !repo_root.join(".git").exists() {
        anyhow::bail!("No Git repository found. Run 'tl init' first.");
    }

    // Remove the remote
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("remote")
        .arg("remove")
        .arg(name)
        .output()
        .context("Failed to execute git remote remove command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("No such remote") {
            anyhow::bail!("Remote '{}' not found", name);
        }

        anyhow::bail!("Git remote remove failed: {}", stderr);
    }

    println!("{} Removed remote: {}", "✓".green(), name.cyan());

    Ok(())
}

/// Change a remote's URL
pub async fn run_set_url(name: &str, new_url: &str, push: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Check if Git repo exists
    if !repo_root.join(".git").exists() {
        anyhow::bail!("No Git repository found. Run 'tl init' first.");
    }

    // Validate URL
    if new_url.is_empty() {
        anyhow::bail!("Remote URL cannot be empty");
    }

    // Set the URL
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("remote")
        .arg("set-url");

    if push {
        cmd.arg("--push");
    }

    cmd.arg(name)
        .arg(new_url);

    let output = cmd.output()
        .context("Failed to execute git remote set-url command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("No such remote") {
            anyhow::bail!("Remote '{}' not found", name);
        }

        anyhow::bail!("Git remote set-url failed: {}", stderr);
    }

    if push {
        println!("{} Updated push URL for {}: {}", "✓".green(), name.cyan(), new_url.bright_blue());
    } else {
        println!("{} Updated URL for {}: {}", "✓".green(), name.cyan(), new_url.bright_blue());
    }

    Ok(())
}

/// Rename a remote
pub async fn run_rename(old_name: &str, new_name: &str) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Check if Git repo exists
    if !repo_root.join(".git").exists() {
        anyhow::bail!("No Git repository found. Run 'tl init' first.");
    }

    // Validate new name
    if new_name.is_empty() {
        anyhow::bail!("Remote name cannot be empty");
    }

    if new_name.contains(' ') || new_name.contains('/') {
        anyhow::bail!("Invalid remote name: {}", new_name);
    }

    // Rename the remote
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("remote")
        .arg("rename")
        .arg(old_name)
        .arg(new_name)
        .output()
        .context("Failed to execute git remote rename command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("No such remote") {
            anyhow::bail!("Remote '{}' not found", old_name);
        }

        if stderr.contains("already exists") {
            anyhow::bail!("Remote '{}' already exists", new_name);
        }

        anyhow::bail!("Git remote rename failed: {}", stderr);
    }

    println!("{} Renamed remote: {} -> {}", "✓".green(), old_name.dimmed(), new_name.cyan());

    Ok(())
}

/// Get the URL of a remote
pub async fn run_get_url(name: &str, push: bool) -> Result<()> {
    let repo_root = util::find_repo_root()?;

    // Check if Git repo exists
    if !repo_root.join(".git").exists() {
        anyhow::bail!("No Git repository found. Run 'tl init' first.");
    }

    // Get the URL
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("remote")
        .arg("get-url");

    if push {
        cmd.arg("--push");
    }

    cmd.arg(name);

    let output = cmd.output()
        .context("Failed to execute git remote get-url command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("No such remote") {
            anyhow::bail!("Remote '{}' not found", name);
        }

        anyhow::bail!("Git remote get-url failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{}", stdout.trim().bright_blue());

    Ok(())
}
