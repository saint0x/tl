//! Initialize Timelapse and underlying VCS layers

use crate::locks::InitLock;
use anyhow::{Context, Result};
use journal::{Checkpoint, CheckpointMeta, CheckpointReason, Journal};
use owo_colors::OwoColorize;
use std::env;
use std::path::Path;
use tl_core::store::{Store, update_vcs_config, update_user_config};
use tl_core::{Entry, Tree};
use watcher::ignore::{IgnoreConfig, IgnoreRules};

pub async fn run(skip_git: bool, skip_jj: bool) -> Result<()> {
    let current_dir = env::current_dir()?;

    println!("{}", "Initializing Timelapse...".bold());
    println!();

    // Phase 0: Acquire init lock to prevent concurrent initializations
    // CRITICAL: This prevents race conditions when multiple processes try to init
    let _init_lock = InitLock::acquire(&current_dir)
        .context("Failed to acquire init lock - is another init in progress?")?;

    // Phase 1: State Detection
    let git_exists = crate::util::detect_git_repo(&current_dir)?;
    let jj_exists = jj::detect_jj_workspace(&current_dir)?.is_some();
    let tl_exists = current_dir.join(".tl").exists();

    // Handle .tl already exists case (re-check after acquiring lock)
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

    // Phase 4.5: Create initial checkpoint of existing files BEFORE daemon starts
    // CRITICAL: This ensures existing files are captured as checkpoint #1
    // Must happen before daemon starts because daemon locks the journal
    // CRITICAL: This is now FATAL - if it fails, we clean up and exit
    println!();
    println!("{} Creating initial checkpoint...", "→".cyan());
    match create_initial_checkpoint(&current_dir) {
        Ok(Some(cp_id)) => {
            let short_id = &cp_id[..8.min(cp_id.len())];
            println!("  {} Initial checkpoint: {}", "✓".green(), short_id.bright_cyan());
        }
        Ok(None) => {
            println!("  {} No files to checkpoint (empty directory)", "→".dimmed());
        }
        Err(e) => {
            // CRITICAL: Initial checkpoint failure is now FATAL
            // Without this, users think files are tracked when they aren't
            println!("  {} Failed to create initial checkpoint: {}", "✗".red(), e);
            println!();
            println!("{}", "Cleaning up incomplete initialization...".red());

            // Clean up the .tl directory we just created
            let tl_dir = current_dir.join(".tl");
            if tl_dir.exists() {
                if let Err(cleanup_err) = std::fs::remove_dir_all(&tl_dir) {
                    println!("  {} Warning: Could not clean up .tl directory: {}", "!".yellow(), cleanup_err);
                    println!("  {} Please remove .tl/ manually before retrying", "→".dimmed());
                }
            }

            anyhow::bail!("Failed to capture existing files. Please check disk space and file permissions, then try again.");
        }
    }

    // Phase 4.6: Auto-start daemon (after initial checkpoint so it can watch for future changes)
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

/// Create initial checkpoint capturing all existing files in the repository
///
/// This is called during `tl init` to ensure that files existing before init
/// are captured in the first checkpoint. Without this, only files that change
/// AFTER init would be tracked (Bug: existing files not captured).
///
/// Returns:
/// - Ok(Some(checkpoint_id)) if files were found and checkpointed
/// - Ok(None) if directory is empty (no files to checkpoint)
/// - Err if checkpoint creation failed
fn create_initial_checkpoint(repo_root: &Path) -> Result<Option<String>> {
    let tl_dir = repo_root.join(".tl");

    // Open store and journal
    let store = Store::open(repo_root)
        .context("Failed to open store for initial checkpoint")?;
    let journal = Journal::open(&tl_dir.join("journal"))
        .context("Failed to open journal for initial checkpoint")?;

    // Load ignore rules to respect .gitignore and .tlignore
    let ignore_config = IgnoreConfig::default();
    let ignore_rules = IgnoreRules::load(repo_root, ignore_config)
        .context("Failed to load ignore rules")?;

    // Build tree from all files in working directory
    let mut tree = Tree::new();
    let mut files_count = 0u32;
    let mut touched_paths = Vec::new();

    for entry in walkdir::WalkDir::new(repo_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip the root directory itself from filtering
            if e.path() == repo_root {
                return true;
            }
            // Use ignore rules to filter
            let rel_path = e.path().strip_prefix(repo_root).unwrap_or(e.path());
            !ignore_rules.should_ignore(rel_path)
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip directories (we only store files)
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let rel_path = match path.strip_prefix(repo_root) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Skip protected directories (double-check)
        let rel_str = rel_path.to_string_lossy();
        if rel_str.starts_with(".tl/") || rel_str.starts_with(".git/") || rel_str.starts_with(".jj/") {
            continue;
        }

        // Hash file content
        let blob_hash = tl_core::hash::hash_file(path)
            .with_context(|| format!("Failed to hash file: {}", path.display()))?;

        // Store blob if not already present
        if !store.blob_store().has_blob(blob_hash) {
            let content = std::fs::read(path)
                .with_context(|| format!("Failed to read file: {}", path.display()))?;
            store.blob_store().write_blob(blob_hash, &content)
                .with_context(|| format!("Failed to store blob for: {}", path.display()))?;
        }

        // Get file mode
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            entry.metadata()
                .map(|m| m.mode())
                .unwrap_or(0o644)
        };
        #[cfg(not(unix))]
        let mode = 0o644;

        // Add to tree
        tree.insert(rel_path, Entry::file(mode, blob_hash));
        touched_paths.push(rel_path.to_path_buf());
        files_count += 1;
    }

    // If no files found, return None
    if files_count == 0 {
        return Ok(None);
    }

    // Compute and store tree
    let tree_hash = tree.hash();
    store.write_tree(&tree)
        .context("Failed to write tree for initial checkpoint")?;

    // Create checkpoint (no parent since this is the first)
    let checkpoint = Checkpoint::new(
        None, // No parent - this is the first checkpoint
        tree_hash,
        CheckpointReason::Manual, // Treat initial snapshot as manual
        touched_paths,
        CheckpointMeta {
            files_changed: files_count,
            bytes_added: 0,    // Could calculate, but not critical
            bytes_removed: 0,
        },
    );

    // Append to journal
    journal.append(&checkpoint)
        .context("Failed to append initial checkpoint to journal")?;

    Ok(Some(checkpoint.id.to_string()))
}
