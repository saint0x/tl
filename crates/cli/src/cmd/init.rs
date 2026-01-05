//! Initialize Timelapse and underlying VCS layers

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::env;
use std::path::Path;
use tl_core::store::{Store, update_vcs_config, update_user_config};

pub async fn run(skip_git: bool, skip_jj: bool) -> Result<()> {
    let current_dir = env::current_dir()?;

    println!("{}", "Initializing Timelapse...".bold());
    println!();

    // Phase 1: State Detection
    let git_exists = crate::util::detect_git_repo(&current_dir)?;
    let jj_exists = jj::detect_jj_workspace(&current_dir)?.is_some();
    let tl_exists = current_dir.join(".tl").exists();

    // Handle .tl already exists case
    if tl_exists {
        println!("{}", "Error: Timelapse already initialized".red());
        println!("Location: {}/.tl/", current_dir.display());
        std::process::exit(1);
    }

    // Phase 2: Git Initialization
    let git_initialized = initialize_git_if_needed(&current_dir, git_exists, skip_git)?;

    // Phase 3: JJ Initialization
    let jj_initialized = initialize_jj_if_needed(
        &current_dir,
        git_initialized || git_exists,
        jj_exists,
        skip_jj,
    )?;

    // Phase 4: Timelapse Initialization
    Store::init(&current_dir)?;

    // Ensure logs directory exists for daemon
    std::fs::create_dir_all(current_dir.join(".tl/logs"))
        .context("Failed to create logs directory")?;

    // Create default .tlignore if it doesn't exist
    let tlignore_path = current_dir.join(".tlignore");
    if !tlignore_path.exists() {
        let tlignore_content = r#"# Timelapse Ignore Patterns
# Built-in patterns already cover: .tl/, .git/, .jj/, editor temp files
# Add project-specific patterns here

# Build outputs (examples)
# /build/
# /dist/
# *.log

# Dependencies (if not already covered)
# /vendor/

# Project-specific
"#;
        std::fs::write(&tlignore_path, tlignore_content)
            .context("Failed to create .tlignore")?;
    }

    // Phase 4.5: Auto-start daemon
    println!();
    println!("{} Starting daemon...", "→".cyan());
    match crate::daemon::ensure_daemon_running().await {
        Ok(()) => {
            println!("  {} Daemon started", "✓".green());
        }
        Err(e) => {
            println!("  {} Warning: Could not auto-start daemon: {}", "!".yellow(), e);
            println!("  {} Start it manually with: tl start", "→".dimmed());
            // Continue - daemon failure is non-fatal for init
        }
    }

    // Phase 4.6: Create seed commit for fast publishing (background)
    // This runs in the background so init doesn't block
    let has_jj = jj_initialized || jj::detect_jj_workspace(&current_dir)?.is_some();
    if has_jj {
        let repo_root = current_dir.clone();
        std::thread::spawn(move || {
            create_seed_commit_background(&repo_root);
        });
        println!("  {} Seed commit creation started (background)", "→".dimmed());
    }

    // Phase 5: Configuration Synchronization
    sync_configurations(&current_dir, git_initialized || git_exists)?;

    // Phase 6: Success Summary
    print_success_summary(&current_dir, git_initialized, jj_initialized);

    Ok(())
}

fn initialize_git_if_needed(
    repo_root: &Path,
    git_exists: bool,
    skip_git: bool,
) -> Result<bool> {
    if git_exists {
        println!("{} Git repository detected", "✓".green());
        return Ok(false); // Already existed
    }

    if skip_git {
        println!(
            "{} Skipping git initialization (--skip-git)",
            "→".yellow()
        );
        return Ok(false);
    }

    println!("{} Initializing git repository...", "→".cyan());

    let output = std::process::Command::new("git")
        .arg("init")
        .current_dir(repo_root)
        .output()
        .context("Failed to run 'git init'. Is git installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git init failed: {}", stderr);
    }

    println!("{} Created .git/ directory", "✓".green());
    Ok(true) // We initialized it
}

fn initialize_jj_if_needed(
    repo_root: &Path,
    git_exists: bool,
    jj_exists: bool,
    skip_jj: bool,
) -> Result<bool> {
    if jj_exists {
        println!("{} JJ workspace detected", "✓".green());
        return Ok(false);
    }

    if skip_jj {
        println!(
            "{} Skipping JJ initialization (--skip-jj)",
            "→".yellow()
        );
        return Ok(false);
    }

    if !git_exists {
        anyhow::bail!(
            "Cannot initialize JJ without git. Run with --skip-jj or initialize git first."
        );
    }

    println!("{} Initializing JJ workspace...", "→".cyan());

    // Use external git mode (link to existing .git)
    jj::init_jj_external(repo_root, &repo_root.join(".git"))
        .context("Failed to initialize JJ with existing git repo")?;

    println!("{} Created .jj/ workspace", "✓".green());

    // Get git user config to configure JJ with same identity
    let (user_name, user_email) = crate::util::parse_git_user_config(repo_root)?
        .map(|(n, e)| (Some(n), Some(e)))
        .unwrap_or((None, None));

    // Configure bookmarks and user identity for timelapse workflow
    if let Err(e) = jj::configure_jj_with_user(
        repo_root,
        user_name.as_deref(),
        user_email.as_deref(),
    ) {
        println!(
            "{} Warning: Could not configure JJ: {}",
            "!".yellow(),
            e
        );
    } else if user_name.is_some() && user_email.is_some() {
        println!(
            "  {} Configured JJ user: {} <{}>",
            "✓".green(),
            user_name.as_ref().unwrap(),
            user_email.as_ref().unwrap()
        );
    }

    Ok(true) // We initialized it
}

fn sync_configurations(repo_root: &Path, git_exists: bool) -> Result<()> {
    if !git_exists {
        return Ok(()); // Nothing to sync
    }

    println!();
    println!("{} Synchronizing configurations...", "→".cyan());

    let tl_dir = repo_root.join(".tl");

    // 1. Update .gitignore
    crate::util::ensure_gitignore_patterns(repo_root, &[".tl/"])
        .context("Failed to update .gitignore")?;
    println!("  {} Updated .gitignore", "✓".green());

    // 2. Sync git user identity
    if let Some((name, email)) = crate::util::parse_git_user_config(repo_root)? {
        update_user_config(&tl_dir, &name, &email)?;
        let name_str = name.clone();
        let email_str = email.clone();
        println!(
            "  {} Synced user identity: {} <{}>",
            "✓".green(),
            name_str,
            email_str
        );
    }

    // 3. Store git remote
    let remotes = crate::util::parse_git_remotes(repo_root)?;
    let primary_remote = remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.first());

    if let Some((remote_name, remote_url)) = primary_remote {
        update_vcs_config(&tl_dir, true, true, Some(remote_url.clone()))?;
        println!(
            "  {} Detected remote '{}': {}",
            "✓".green(),
            remote_name,
            remote_url
        );
    } else {
        update_vcs_config(&tl_dir, true, true, None)?;
    }

    Ok(())
}

fn print_success_summary(repo_root: &Path, git_initialized: bool, jj_initialized: bool) {
    println!();
    println!(
        "{}",
        "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".dimmed()
    );
    println!("{} Timelapse initialized successfully!", "✓".green().bold());
    println!();

    println!("VCS Stack:");
    if git_initialized {
        println!("  {} Git repository (created)", "✓".green());
    } else {
        println!("  {} Git repository (existing)", "✓".green());
    }

    if jj_initialized {
        println!("  {} JJ workspace (created)", "✓".green());
    } else if jj::detect_jj_workspace(repo_root).unwrap_or(None).is_some() {
        println!("  {} JJ workspace (existing)", "✓".green());
    }

    println!("  {} Timelapse store (created)", "✓".green());
    println!();

    println!("Directory structure:");
    if git_initialized || repo_root.join(".git").exists() {
        println!("  {}/.git/          (git repository)", repo_root.display());
    }
    if jj_initialized || jj::detect_jj_workspace(repo_root).unwrap_or(None).is_some() {
        println!("  {}/.jj/           (jj workspace)", repo_root.display());
    }
    println!("  {}/.tl/           (timelapse store)", repo_root.display());
    println!();

    println!("Next steps:");
    println!(
        "  {} tl status      - Check daemon and checkpoint status",
        "→".cyan()
    );
    println!(
        "  {} tl log         - View checkpoint timeline",
        "→".cyan()
    );
    println!(
        "  {} tl info        - View repository information",
        "→".cyan()
    );
    println!();
    println!(
        "{}",
        "Happy coding! Your changes are now being tracked.".dimmed()
    );
}

/// Create seed commit in background thread
///
/// This enables fast incremental publishing for the first publish.
/// Runs in background so init doesn't block.
fn create_seed_commit_background(repo_root: &Path) {
    let tl_dir = repo_root.join(".tl");

    match jj::create_and_store_seed(repo_root, &tl_dir) {
        Ok(commit_id) => {
            // Log to file since we're in background
            let log_path = tl_dir.join("logs/seed.log");
            let _ = std::fs::write(
                &log_path,
                format!("Seed commit created: {}\n", &commit_id[..12]),
            );
        }
        Err(e) => {
            // Non-fatal - old repos can still publish, just slower for first time
            let log_path = tl_dir.join("logs/seed.log");
            let _ = std::fs::write(
                &log_path,
                format!("Seed commit failed (non-fatal): {}\n", e),
            );
        }
    }
}
