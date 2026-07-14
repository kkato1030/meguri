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
    Doctor {
        /// Also fire a one-shot live probe per agent profile to verify each
        /// model alias still resolves (spends a few hundred tokens of quota)
        #[arg(long)]
        probe: bool,
    },
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
    /// Queue a local task (local-mode projects; watch picks it up)
    Add {
        /// Project id from config.toml (defaults to the sole configured project)
        #[arg(long)]
        project: Option<String>,
        /// Queue it for the planner instead of the worker
        #[arg(long)]
        plan: bool,
        /// Read the task from a markdown file (first heading → title, body → body)
        #[arg(long)]
        file: Option<String>,
        /// Hold the task until this instant (YYYY-MM-DD or RFC3339 UTC); it is
        /// discovered only once the time passes (issue #148)
        #[arg(long)]
        not_before: Option<String>,
        /// Task title (omit only when --file supplies a heading)
        title: Option<String>,
    },
    /// List local tasks (needs_human is highlighted)
    Tasks {
        /// Project id from config.toml (defaults to the sole configured project)
        #[arg(long)]
        project: Option<String>,
        /// Include done/cancelled tasks
        #[arg(long)]
        all: bool,
    },
    /// List cron schedules (definition, last fire, next fire)
    Schedules {
        /// Project id from config.toml (defaults to the sole configured project)
        #[arg(long)]
        project: Option<String>,
    },
    /// List runs and their interaction state
    Ps {
        /// Include finished runs
        #[arg(long)]
        all: bool,
    },
    /// Show aggregate stats read straight from sqlite (works with watch stopped)
    Stats {
        #[command(subcommand)]
        command: StatsCommand,
    },
    /// Build a dedicated dashboard workspace of tiled live agent panes and
    /// attach to it — a terminal dashboard
    Top {
        /// Multiplexer override: herdr | tmux
        #[arg(long)]
        mux: Option<String>,
        /// Status refresh interval in seconds
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
    /// (internal) status-render loop for `meguri top`, run inside its status
    /// pane. Not for direct use.
    #[command(hide = true)]
    TopStatus {
        /// Multiplexer override: herdr | tmux
        #[arg(long)]
        mux: Option<String>,
        /// Dashboard tiling container id the outer `top` created
        #[arg(long)]
        dashboard: String,
        /// Status refresh interval in seconds
        #[arg(long, default_value_t = 2)]
        interval: u64,
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
    /// Distribute the embedded meguri skill/rule fragment to agent CLIs, so
    /// an agent working nearby can learn about and propose meguri on its own
    AgentSkills {
        #[command(subcommand)]
        command: AgentSkillsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentSkillsCommand {
    /// Install the user-level skill (default), or the project-level rule
    /// fragment with `--project`
    Install {
        /// Agent CLI to target (currently only "claude")
        #[arg(long, default_value = "claude")]
        target: String,
        /// Install the repo-level rule fragment (`.claude/rules/meguri.md`
        /// for the claude target) instead of the user-level skill
        /// (`~/.claude/skills/meguri/`)
        #[arg(long)]
        project: bool,
        /// Repo root for --project (defaults to the current directory)
        #[arg(long)]
        repo: Option<String>,
        /// Overwrite files that differ from the embedded source (without
        /// this, a diff is shown and differing files are left untouched)
        #[arg(long)]
        force: bool,
    },
    /// Show whether the skill/rule fragment is installed and matches this
    /// binary's embedded version
    Status {
        /// Agent CLI to target (currently only "claude")
        #[arg(long, default_value = "claude")]
        target: String,
        /// Check the project-level rule fragment instead of the user-level
        /// skill
        #[arg(long)]
        project: bool,
        /// Repo root for --project (defaults to the current directory)
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum StatsCommand {
    /// Success rate / mean turns / mean duration per (role, profile), plus any
    /// active routing drift
    Routing {
        /// Restrict to one project id (default: all projects, project column)
        #[arg(long)]
        project: Option<String>,
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

    #[test]
    fn agent_skills_install_defaults_to_user_level_claude_target() {
        let cli = Cli::try_parse_from(["meguri", "agent-skills", "install"]).unwrap();
        match cli.command {
            Command::AgentSkills {
                command:
                    AgentSkillsCommand::Install {
                        target,
                        project,
                        repo,
                        force,
                    },
            } => {
                assert_eq!(target, "claude");
                assert!(!project);
                assert_eq!(repo, None);
                assert!(!force);
            }
            other => panic!("expected AgentSkills(Install), got {other:?}"),
        }
    }

    #[test]
    fn agent_skills_install_parses_project_flags() {
        let cli = Cli::try_parse_from([
            "meguri",
            "agent-skills",
            "install",
            "--project",
            "--repo",
            "/tmp/some-repo",
            "--force",
        ])
        .unwrap();
        match cli.command {
            Command::AgentSkills {
                command:
                    AgentSkillsCommand::Install {
                        project,
                        repo,
                        force,
                        ..
                    },
            } => {
                assert!(project);
                assert_eq!(repo.as_deref(), Some("/tmp/some-repo"));
                assert!(force);
            }
            other => panic!("expected AgentSkills(Install), got {other:?}"),
        }
    }

    #[test]
    fn agent_skills_status_parses() {
        let cli = Cli::try_parse_from(["meguri", "agent-skills", "status", "--project"]).unwrap();
        match cli.command {
            Command::AgentSkills {
                command:
                    AgentSkillsCommand::Status {
                        target, project, ..
                    },
            } => {
                assert_eq!(target, "claude");
                assert!(project);
            }
            other => panic!("expected AgentSkills(Status), got {other:?}"),
        }
    }
}
