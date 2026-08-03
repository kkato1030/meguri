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
        /// model alias still resolves (spends a few hundred tokens of quota),
        /// and a short interactive PTY probe per pane-launched profile to
        /// check the bypass-permissions gate is already accepted
        #[arg(long)]
        probe: bool,
    },
    /// Capture work with one line: a GitHub issue (github mode — created
    /// immediately, then refined by an agent best-effort) or a local task
    /// (local mode — queued for the watch)
    Add {
        /// The one-line memo / task title (in local mode, omit only when
        /// --file supplies a heading)
        text: Option<String>,
        /// Project id from config.toml (default: inferred from the cwd, or the
        /// sole configured project)
        #[arg(long)]
        project: Option<String>,
        /// github mode: also queue it for the worker loop (`meguri:ready`)
        #[arg(long)]
        ready: bool,
        /// local mode: read the task from a markdown file (first heading →
        /// title, body → body)
        #[arg(long)]
        file: Option<String>,
    },
    /// Add a project to config.toml in one command: append a [[projects]]
    /// entry (and materialize its managed clone). github mode takes an
    /// owner/repo; local mode takes --local <path>.
    AddProject {
        /// owner/repo on GitHub (github mode). Required unless --local.
        #[arg(
            value_name = "owner/repo",
            required_unless_present = "local",
            conflicts_with = "local"
        )]
        slug: Option<String>,
        /// Create the repo from scratch first (`gh repo create`, initial commit
        /// included). Irreversible — meguri never deletes a repo it created.
        #[arg(long, conflicts_with = "local")]
        create: bool,
        /// Visibility for --create (default: private). Requires --create.
        #[arg(long, requires = "create", conflicts_with = "local")]
        public: bool,
        /// Override the derived project id (default: the repo name, or the
        /// --local path's basename)
        #[arg(long)]
        id: Option<String>,
        /// Add a local-mode project rooted at this absolute path (no GitHub;
        /// repo_slug not required)
        #[arg(long, value_name = "path")]
        local: Option<String>,
    },
    /// Run the foreground orchestrator (poll GitHub, drive runs)
    Watch,
    /// Run one identity now: the owning reconciler decides the role
    /// (ADR 0016). Exactly one of --issue / --pr / --run / --task.
    Run {
        /// Project id from config.toml (defaults to the sole configured project)
        #[arg(long)]
        project: Option<String>,
        /// Issue number (issue identity; an open meguri PR re-routes to it)
        #[arg(long)]
        issue: Option<i64>,
        /// PR number (PR identity — the PR-side decider)
        #[arg(long)]
        pr: Option<i64>,
        /// Existing run id (resumes with its stored loop kind)
        #[arg(long)]
        run: Option<String>,
        /// Local task id (local identity)
        #[arg(long)]
        task: Option<i64>,
        /// Multiplexer override: herdr | tmux
        #[arg(long)]
        mux: Option<String>,
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
    /// List runs and their interaction state
    Ps {
        /// Include finished runs
        #[arg(long)]
        all: bool,
    },
    /// Show events (and recent pane output) for a run
    Logs { run: String },
    /// Attach your terminal to an identity's live pane (ADR 0016)
    Attach {
        /// Issue number or run id (positional, back-compat)
        run: Option<String>,
        /// Issue number (issue identity)
        #[arg(long)]
        issue: Option<i64>,
        /// PR number — resolved to its canonical issue's pane
        #[arg(long)]
        pr: Option<i64>,
        /// Run id
        #[arg(long = "run-id")]
        run_id: Option<String>,
        /// Local task id — resolved to its latest run's pane
        #[arg(long)]
        task: Option<i64>,
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
    fn add_project_github_form_parses() {
        let cli = Cli::try_parse_from(["meguri", "add-project", "owner/repo"]).unwrap();
        match cli.command {
            Command::AddProject {
                slug,
                create,
                public,
                id,
                local,
            } => {
                assert_eq!(slug.as_deref(), Some("owner/repo"));
                assert!(!create && !public);
                assert_eq!(id, None);
                assert_eq!(local, None);
            }
            other => panic!("expected AddProject, got {other:?}"),
        }
    }

    #[test]
    fn add_project_local_form_parses_without_positional() {
        let cli = Cli::try_parse_from(["meguri", "add-project", "--local", "/abs/path"]).unwrap();
        match cli.command {
            Command::AddProject { slug, local, .. } => {
                assert_eq!(slug, None);
                assert_eq!(local.as_deref(), Some("/abs/path"));
            }
            other => panic!("expected AddProject, got {other:?}"),
        }
    }

    #[test]
    fn add_project_requires_slug_or_local() {
        // Neither positional nor --local → clap rejects (required_unless_present).
        assert!(Cli::try_parse_from(["meguri", "add-project"]).is_err());
    }

    #[test]
    fn add_project_slug_and_local_conflict() {
        assert!(
            Cli::try_parse_from(["meguri", "add-project", "owner/repo", "--local", "/p"]).is_err()
        );
    }

    #[test]
    fn add_project_create_conflicts_with_local() {
        assert!(
            Cli::try_parse_from(["meguri", "add-project", "--local", "/p", "--create"]).is_err()
        );
    }

    #[test]
    fn add_project_public_requires_create() {
        assert!(Cli::try_parse_from(["meguri", "add-project", "owner/repo", "--public"]).is_err());
        // With --create it is accepted.
        assert!(
            Cli::try_parse_from([
                "meguri",
                "add-project",
                "owner/repo",
                "--create",
                "--public"
            ])
            .is_ok()
        );
    }
}
