//! Timelapse CLI - tl command

use clap::{Parser, Subcommand};
use anyhow::Result;

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
    Init,
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
    /// Publish checkpoint to JJ
    Publish {
        /// Checkpoint ID or range
        checkpoint: String,
    },
    /// Push to Git via JJ
    Push {
        /// Bookmark name
        bookmark: String,
    },
    /// Pull from Git via JJ
    Pull,
    /// Start the daemon
    Start {
        /// Run in foreground (for debugging)
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the daemon
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd::init::run().await,
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
        Commands::Publish { checkpoint } => cmd::publish::run(&checkpoint).await,
        Commands::Push { bookmark } => cmd::push::run(&bookmark).await,
        Commands::Pull => cmd::pull::run().await,
        Commands::Start { foreground } => cmd::start::run(foreground).await,
        Commands::Stop => cmd::stop::run().await,
    }
}
