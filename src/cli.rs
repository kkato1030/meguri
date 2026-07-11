use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "meguri",
    version,
    about = "AI dev loops inside your terminal multiplexer — attach and intervene anytime."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create ~/.meguri (config.toml + sqlite db)
    Init,
    /// Check environment: gh auth, mux availability, git
    Doctor,
    /// Run the foreground orchestrator (poll GitHub, drive runs)
    Watch,
    /// Run the worker loop once for a single issue
    Run {
        /// Project id from config.toml (defaults to the sole configured project)
        #[arg(long)]
        project: Option<String>,
        /// Issue number to work on
        #[arg(long)]
        issue: i64,
        /// Multiplexer override: herdr | tmux
        #[arg(long)]
        mux: Option<String>,
    },
    /// List runs and their interaction state
    Ps {
        /// Include finished runs
        #[arg(long)]
        all: bool,
    },
    /// Show events (and recent pane output) for a run
    Logs { run: String },
    /// Attach your terminal to the run's pane
    Attach { run: String },
    /// Stop injecting prompts; keep the pane alive
    Pause { run: String },
    /// Resume a paused run
    Resume { run: String },
    /// Take over the pane: orchestrator goes hands-off until handback
    Takeover { run: String },
    /// Hand control back to the orchestrator after a takeover
    Handback { run: String },
    /// Kill the pane and cancel the run
    Stop { run: String },
    /// Reclaim panes and worktrees (and merged local branches) of closed
    /// issues; agent session ids are saved first so panes stay resumable
    Clean {
        /// Only clean this project (default: all configured projects)
        #[arg(long)]
        project: Option<String>,
        /// List what would be reclaimed without removing anything
        #[arg(long)]
        dry_run: bool,
        /// Also reclaim dirty worktrees and force-delete unmerged branches
        #[arg(long)]
        force: bool,
    },
}
