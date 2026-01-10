//! Timelapse CLI - tl command

use clap::{Parser, Subcommand};
use anyhow::Result;
use std::path::PathBuf;

mod cmd;
mod daemon;
mod data_access;
mod diff_utils;
mod ipc;
mod locks;
mod system_config;
mod util;

/// Timelapse - Lossless checkpoint stream for your code
#[derive(Parser)]
#[command(name = "tl")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize Timelapse in the current repository
    Init {
        /// Skip git initialization even if .git doesn't exist
        #[arg(long)]
        skip_git: bool,

        /// Skip JJ initialization even if .jj doesn't exist
        #[arg(long)]
        skip_jj: bool,
    },
    /// Show daemon and checkpoint status
    Status {
        /// Show remote branch status (ahead/behind)
        #[arg(short, long)]
        remote: bool,
    },
    /// Show detailed repository information
    Info,
    /// Show checkpoint timeline
    Log {
        /// Number of checkpoints to show (default: 20)
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show detailed checkpoint information
    Show {
        /// Checkpoint ID or label
        checkpoint: String,
        /// Show diff with parent
        #[arg(short = 'p', long)]
        diff: bool,
    },
    /// Show diff between checkpoints
    Diff {
        /// First checkpoint ID
        checkpoint_a: String,
        /// Second checkpoint ID
        checkpoint_b: String,
        /// Show line-by-line diff (default: file list only)
        #[arg(short = 'p', long)]
        patch: bool,
        /// Number of context lines (default: 3)
        #[arg(short = 'U', long, default_value = "3")]
        context: usize,
        /// Maximum files to show line diffs for (default: 10)
        #[arg(long, default_value = "10")]
        max_files: usize,
    },
    /// Restore working tree to a checkpoint
    Restore {
        /// Checkpoint ID or label
        checkpoint: String,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Pin a checkpoint with a name
    Pin {
        /// Checkpoint ID
        checkpoint: String,
        /// Pin name
        name: String,
    },
    /// Remove a pin
    Unpin {
        /// Pin name
        name: String,
    },
    /// Run garbage collection
    Gc,
    /// Publish checkpoint(s) to JJ
    Publish {
        /// Checkpoint ID or range (e.g., HEAD or HEAD~10..HEAD)
        checkpoint: String,
        /// Bookmark name (will be prefixed with tl/)
        #[arg(short, long)]
        bookmark: Option<String>,
        /// Compact range into single commit (default: expand)
        #[arg(long)]
        compact: bool,
        /// Don't auto-pin published checkpoints
        #[arg(long)]
        no_pin: bool,
        /// Custom commit message template
        #[arg(long)]
        message_template: Option<String>,
    },
    /// Push to Git remote via JJ
    Push {
        /// Bookmark name (optional, will be prefixed with tl/)
        #[arg(short, long)]
        bookmark: Option<String>,
        /// Push all tl/* bookmarks
        #[arg(long)]
        all: bool,
        /// Force push
        #[arg(long)]
        force: bool,
    },
    /// Pull from Git remote via JJ
    Pull {
        /// Only fetch, don't import
        #[arg(long)]
        fetch_only: bool,
        /// Don't auto-pin imported checkpoints
        #[arg(long)]
        no_pin: bool,
    },
    /// Fetch from Git remote and sync working directory
    Fetch {
        /// Don't sync working directory after fetch
        #[arg(long)]
        no_sync: bool,
        /// Remove branches that have been deleted on remote
        #[arg(long)]
        prune: bool,
    },
    /// List, create, or delete branches
    Branch {
        /// Show remote branches
        #[arg(short, long)]
        remote: bool,
        /// Show all branches (local + remote)
        #[arg(short, long)]
        all: bool,
        /// Delete a branch
        #[arg(short, long)]
        delete: Option<String>,
        /// Branch name to create (requires checkpoint argument)
        #[arg(long)]
        create: Option<String>,
        /// Checkpoint to create branch at (used with --create)
        #[arg(long)]
        at: Option<String>,
    },
    /// Merge changes from a branch
    Merge {
        /// Branch to merge (e.g., tl/main)
        branch: Option<String>,
        /// Abort the in-progress merge
        #[arg(long)]
        abort: bool,
        /// Continue merge after resolving conflicts
        #[arg(long = "continue")]
        continue_merge: bool,
    },
    /// Check and manage conflict resolution
    Resolve {
        /// List files with resolution status
        #[arg(short, long)]
        list: bool,
        /// Continue merge after resolving (shortcut for merge --continue)
        #[arg(long = "continue")]
        continue_merge: bool,
        /// Abort merge (shortcut for merge --abort)
        #[arg(long)]
        abort: bool,
    },
    /// Start the daemon
    Start {
        /// Run in foreground (for debugging)
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the daemon
    Stop,
    /// Force checkpoint creation immediately
    Flush,
    /// Manage JJ workspaces with timelapse integration
    #[command(subcommand)]
    Worktree(WorktreeCommands),
    /// View and manage configuration
    Config {
        /// List all configuration values
        #[arg(short, long)]
        list: bool,
        /// Get a configuration value
        #[arg(short, long)]
        get: Option<String>,
        /// Set a configuration value (format: key=value)
        #[arg(short, long)]
        set: Option<String>,
        /// Show config file path
        #[arg(short, long)]
        path: bool,
        /// Create config file if it doesn't exist (with --path)
        #[arg(long)]
        create: bool,
        /// Show example configuration
        #[arg(short, long)]
        example: bool,
    },
    /// Manage Git tags
    #[command(subcommand)]
    Tag(TagCommands),
    /// Manage stashes (save and restore work-in-progress)
    #[command(subcommand)]
    Stash(StashCommands),
    /// Manage Git remotes
    #[command(subcommand)]
    Remote(RemoteCommands),
}

#[derive(Subcommand)]
enum TagCommands {
    /// List all tags
    List,
    /// Create a new tag
    Create {
        /// Tag name
        name: String,
        /// Checkpoint to tag (default: HEAD)
        #[arg(long)]
        checkpoint: Option<String>,
        /// Annotated tag message
        #[arg(short, long)]
        message: Option<String>,
        /// Force overwrite existing tag
        #[arg(short, long)]
        force: bool,
    },
    /// Delete a tag
    Delete {
        /// Tag name
        name: String,
    },
    /// Show tag details
    Show {
        /// Tag name
        name: String,
    },
    /// Push tags to remote
    Push {
        /// Tag name (optional)
        name: Option<String>,
        /// Push all tags
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum StashCommands {
    /// List all stashes
    List,
    /// Save working changes to stash
    Push {
        /// Stash message
        #[arg(short, long)]
        message: Option<String>,
        /// Include untracked files
        #[arg(short = 'u', long)]
        include_untracked: bool,
    },
    /// Apply a stash to working directory
    Apply {
        /// Stash reference (e.g., stash@{0} or stash/name)
        stash: Option<String>,
    },
    /// Apply and remove a stash
    Pop {
        /// Stash reference (e.g., stash@{0} or stash/name)
        stash: Option<String>,
    },
    /// Delete a stash
    Drop {
        /// Stash reference (e.g., stash@{0} or stash/name)
        stash: Option<String>,
    },
    /// Delete all stashes
    Clear {
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum RemoteCommands {
    /// List all remotes
    List {
        /// Show URLs
        #[arg(short, long)]
        verbose: bool,
    },
    /// Add a new remote
    Add {
        /// Remote name
        name: String,
        /// Remote URL
        url: String,
        /// Fetch immediately after adding
        #[arg(short, long)]
        fetch: bool,
    },
    /// Remove a remote
    Remove {
        /// Remote name
        name: String,
    },
    /// Change remote URL
    SetUrl {
        /// Remote name
        name: String,
        /// New URL
        url: String,
        /// Set push URL (instead of fetch URL)
        #[arg(long)]
        push: bool,
    },
    /// Rename a remote
    Rename {
        /// Old remote name
        old_name: String,
        /// New remote name
        new_name: String,
    },
    /// Get remote URL
    GetUrl {
        /// Remote name
        name: String,
        /// Get push URL (instead of fetch URL)
        #[arg(long)]
        push: bool,
    },
}

#[derive(Subcommand)]
enum WorktreeCommands {
    /// List all workspaces
    List,

    /// Add a new workspace
    Add {
        /// Workspace name
        name: String,

        /// Custom path (default: ../{repo-name}-{name})
        #[arg(long)]
        path: Option<PathBuf>,

        /// Start from specific checkpoint
        #[arg(long)]
        from: Option<String>,

        /// Don't auto-checkpoint current workspace
        #[arg(long)]
        no_checkpoint: bool,
    },

    /// Remove a workspace
    Remove {
        /// Workspace name
        name: String,

        /// Delete workspace files (not just JJ metadata)
        #[arg(long)]
        delete_files: bool,

        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Switch to a workspace
    Switch {
        /// Workspace name
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { skip_git, skip_jj } => cmd::init::run(skip_git, skip_jj).await,
        Commands::Status { remote } => cmd::status::run(remote).await,
        Commands::Info => cmd::info::run().await,
        Commands::Log { limit } => cmd::log::run(limit).await,
        Commands::Show { checkpoint, diff } => {
            cmd::show::run(&checkpoint, diff).await
        }
        Commands::Diff { checkpoint_a, checkpoint_b, patch, context, max_files } => {
            cmd::diff::run(&checkpoint_a, &checkpoint_b, patch, context, max_files).await
        }
        Commands::Restore { checkpoint, yes } => cmd::restore::run(&checkpoint, yes).await,
        Commands::Pin { checkpoint, name } => cmd::pin::run(&checkpoint, &name).await,
        Commands::Unpin { name } => cmd::unpin::run(&name).await,
        Commands::Gc => cmd::gc::run().await,
        Commands::Publish { checkpoint, bookmark, compact, no_pin, message_template } => {
            cmd::publish::run(&checkpoint, bookmark, compact, no_pin, message_template).await
        }
        Commands::Push { bookmark, all, force } => {
            cmd::push::run(bookmark, all, force).await
        }
        Commands::Pull { fetch_only, no_pin } => {
            cmd::pull::run(fetch_only, no_pin).await
        }
        Commands::Fetch { no_sync, prune } => {
            cmd::fetch::run(no_sync, prune).await
        }
        Commands::Branch { remote, all, delete, create, at } => {
            let create_pair = match (create, at) {
                (Some(name), Some(checkpoint)) => Some((name, checkpoint)),
                (Some(_), None) => anyhow::bail!("--create requires --at <checkpoint>"),
                (None, Some(_)) => anyhow::bail!("--at requires --create <branch-name>"),
                (None, None) => None,
            };
            cmd::branch::run(remote, all, delete, create_pair).await
        }
        Commands::Merge { branch, abort, continue_merge } => {
            cmd::merge::run(branch, abort, continue_merge).await
        }
        Commands::Resolve { list, continue_merge, abort } => {
            cmd::resolve::run(list, continue_merge, abort).await
        }
        Commands::Start { foreground } => cmd::start::run(foreground).await,
        Commands::Stop => cmd::stop::run().await,
        Commands::Flush => cmd::flush::execute().await,
        Commands::Worktree(worktree_cmd) => match worktree_cmd {
            WorktreeCommands::List => cmd::worktree_list::run().await,
            WorktreeCommands::Add { name, path, from, no_checkpoint } => {
                cmd::worktree_add::run(&name, path.clone(), from.clone(), no_checkpoint).await
            }
            WorktreeCommands::Remove { name, delete_files, yes } => {
                cmd::worktree_remove::run(&name, delete_files, yes).await
            }
            WorktreeCommands::Switch { name } => {
                cmd::worktree_switch::run(&name).await
            }
        },
        Commands::Config { list, get, set, path, create, example } => {
            if example {
                cmd::config::run_example().await
            } else if path {
                cmd::config::run_path(create).await
            } else if let Some(key) = get {
                cmd::config::run_get(&key).await
            } else if let Some(kv) = set {
                let parts: Vec<&str> = kv.splitn(2, '=').collect();
                if parts.len() != 2 {
                    anyhow::bail!("Invalid format. Use: --set key=value");
                }
                cmd::config::run_set(parts[0], parts[1]).await
            } else if list {
                cmd::config::run_list().await
            } else {
                // Default to list
                cmd::config::run_list().await
            }
        },
        Commands::Tag(tag_cmd) => match tag_cmd {
            TagCommands::List => cmd::tag::run_list().await,
            TagCommands::Create { name, checkpoint, message, force } => {
                cmd::tag::run_create(&name, checkpoint, message, force).await
            }
            TagCommands::Delete { name } => cmd::tag::run_delete(&name).await,
            TagCommands::Show { name } => cmd::tag::run_show(&name).await,
            TagCommands::Push { name, all } => cmd::tag::run_push(name, all).await,
        },
        Commands::Stash(stash_cmd) => match stash_cmd {
            StashCommands::List => cmd::stash::run_list().await,
            StashCommands::Push { message, include_untracked } => {
                cmd::stash::run_push(message, include_untracked).await
            }
            StashCommands::Apply { stash } => cmd::stash::run_apply(stash, false).await,
            StashCommands::Pop { stash } => cmd::stash::run_apply(stash, true).await,
            StashCommands::Drop { stash } => cmd::stash::run_drop(stash).await,
            StashCommands::Clear { yes } => cmd::stash::run_clear(yes).await,
        },
        Commands::Remote(remote_cmd) => match remote_cmd {
            RemoteCommands::List { verbose } => cmd::remote::run_list(verbose).await,
            RemoteCommands::Add { name, url, fetch } => {
                cmd::remote::run_add(&name, &url, fetch).await
            }
            RemoteCommands::Remove { name } => cmd::remote::run_remove(&name).await,
            RemoteCommands::SetUrl { name, url, push } => {
                cmd::remote::run_set_url(&name, &url, push).await
            }
            RemoteCommands::Rename { old_name, new_name } => {
                cmd::remote::run_rename(&old_name, &new_name).await
            }
            RemoteCommands::GetUrl { name, push } => {
                cmd::remote::run_get_url(&name, push).await
            }
        },
    }
}
