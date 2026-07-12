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
    /// Manage the resident watch: detach, OS supervision, status, logs
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
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
    /// Attach your terminal to an issue's pane (or a run's)
    Attach {
        /// Issue number or run id
        run: String,
        /// Attach the issue's review-lane pane instead of the author pane
        #[arg(long)]
        review: bool,
    },
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
    #[command(alias = "clean")]
    Prune {
        /// Only prune this project (default: all configured projects)
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

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start `meguri watch` detached from this terminal
    Start,
    /// Stop the watch (launchd mode: bootout, so it stays down)
    Stop,
    /// Restart the watch, keeping its supervision mode
    Restart,
    /// Show PID / mode / liveness / log location
    Status,
    /// Tail the daemon log
    Logs {
        /// Keep following the log (tail -f)
        #[arg(short, long)]
        follow: bool,
    },
    /// Install OS supervision (generate + bootstrap a LaunchAgent)
    Install {
        /// Supervision mode: launchd (macOS only)
        #[arg(long)]
        mode: String,
    },
    /// Remove OS supervision (bootout + delete the LaunchAgent)
    Uninstall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn prune_parses_with_flags() {
        let cli = Cli::try_parse_from(["meguri", "prune", "--dry-run", "--force"]).unwrap();
        match cli.command {
            Command::Prune {
                project,
                dry_run,
                force,
            } => {
                assert_eq!(project, None);
                assert!(dry_run);
                assert!(force);
            }
            other => panic!("expected Prune, got {other:?}"),
        }
    }

    #[test]
    fn clean_is_a_hidden_alias_for_prune() {
        let cli = Cli::try_parse_from(["meguri", "clean", "--force"]).unwrap();
        assert!(matches!(cli.command, Command::Prune { force: true, .. }));
        // Hidden alias: `clean` must not surface in the top-level help.
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("prune"));
        assert!(!help.contains("clean"));
    }
}
