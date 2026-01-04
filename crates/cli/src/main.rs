//! Timelapse CLI - tl command

use clap::{Parser, Subcommand};
use anyhow::Result;
use std::path::PathBuf;

mod cmd;
mod daemon;
mod ipc;
mod locks;
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
    Status,
    /// Show detailed repository information
    Info,
    /// Show checkpoint timeline
    Log {
        /// Number of checkpoints to show (default: 20)
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show diff between checkpoints
    Diff {
        /// First checkpoint ID
        checkpoint_a: String,
        /// Second checkpoint ID
        checkpoint_b: String,
    },
    /// Restore working tree to a checkpoint
    Restore {
        /// Checkpoint ID or label
        checkpoint: String,
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
        /// Bookmark name (will be prefixed with snap/)
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
        /// Bookmark name (optional, will be prefixed with snap/)
        #[arg(short, long)]
        bookmark: Option<String>,
        /// Push all snap/* bookmarks
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
        Commands::Status => cmd::status::run().await,
        Commands::Info => cmd::info::run().await,
        Commands::Log { limit } => cmd::log::run(limit).await,
        Commands::Diff { checkpoint_a, checkpoint_b } => {
            cmd::diff::run(&checkpoint_a, &checkpoint_b).await
        }
        Commands::Restore { checkpoint } => cmd::restore::run(&checkpoint).await,
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
    }
}
