use anyhow::{Result, bail};
use clap::Parser;
use meguri::app;
use meguri::cli::{Cli, Command, DaemonCommand};
use meguri::config::{self, Config};
use meguri::daemon;
use meguri::store::Store;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MEGURI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("meguri=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init(),
        Command::Doctor => cmd_doctor().await,
        Command::Watch => app::cmd_watch().await,
        Command::Daemon { command } => match command {
            DaemonCommand::Start => daemon::cmd_start(),
            DaemonCommand::Stop => daemon::cmd_stop(),
            DaemonCommand::Restart => daemon::cmd_restart(),
            DaemonCommand::Status => daemon::cmd_status(),
            DaemonCommand::Logs { follow } => daemon::cmd_logs(follow),
            DaemonCommand::Install { mode } => daemon::launchd::cmd_install(&mode),
            DaemonCommand::Uninstall => daemon::launchd::cmd_uninstall(),
        },
        Command::Serve { port, bind } => app::cmd_serve(port, bind.as_deref()).await,
        Command::Run {
            project,
            issue,
            mux,
        } => app::cmd_run(project.as_deref(), issue, mux.as_deref()).await,
        Command::Ps { all } => app::cmd_ps(all),
        Command::Logs { run } => app::cmd_logs(&run).await,
        Command::Attach { run } => app::cmd_attach(&run),
        Command::Pause { run } => app::cmd_pause(&run),
        Command::Resume { run } => app::cmd_resume(&run),
        Command::Takeover { run } => app::cmd_takeover(&run),
        Command::Handback { run } => app::cmd_handback(&run),
        Command::Stop { run } => app::cmd_stop(&run).await,
        Command::Prune {
            project,
            dry_run,
            force,
        } => app::cmd_prune(project.as_deref(), dry_run, force).await,
    }
}

fn cmd_init() -> Result<()> {
    let cfg_path = config::config_path();
    if cfg_path.exists() {
        println!("config already exists: {}", cfg_path.display());
    } else {
        if let Some(dir) = cfg_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&cfg_path, config::INIT_TEMPLATE)?;
        println!("wrote {}", cfg_path.display());
    }
    let db = config::db_path();
    Store::open(&db)?;
    println!("db ready: {}", db.display());
    std::fs::create_dir_all(config::worktrees_root())?;
    println!(
        "\nNext: edit {} — fill in the [[projects]] stub (repo_path, repo_slug).",
        cfg_path.display()
    );
    Ok(())
}

async fn cmd_doctor() -> Result<()> {
    let mut ok = true;

    let check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}: {detail}", if pass { "✅" } else { "❌" });
        pass
    };

    let git = run_capture("git", &["--version"]);
    ok &= check("git", git.is_ok(), git.unwrap_or_else(|e| e));

    let gh = run_capture("gh", &["--version"]);
    let gh_present = gh.is_ok();
    ok &= check(
        "gh",
        gh_present,
        gh.map(|v| v.lines().next().unwrap_or_default().to_string())
            .unwrap_or_else(|e| e),
    );
    if gh_present {
        let auth = run_capture("gh", &["auth", "status"]);
        ok &= check(
            "gh auth",
            auth.is_ok(),
            auth.map(|_| "authenticated".into()).unwrap_or_else(|e| e),
        );
    }

    let herdr = run_capture("herdr", &["--version"]);
    let herdr_sock = std::env::var("HERDR_SOCKET_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".config/herdr/herdr.sock")
        });
    let herdr_live = herdr_sock.exists();
    check(
        "herdr",
        herdr.is_ok(),
        match (&herdr, herdr_live) {
            (Ok(v), true) => format!("{v} (socket live: {})", herdr_sock.display()),
            (Ok(v), false) => format!("{v} (socket not found — start `herdr` first)"),
            (Err(e), _) => e.clone(),
        },
    );

    let tmux = run_capture("tmux", &["-V"]);
    let tmux_present = tmux.is_ok();
    check("tmux", tmux_present, tmux.unwrap_or_else(|e| e));

    if !herdr_live && !tmux_present {
        println!("❌ no usable multiplexer (need a running herdr or installed tmux)");
        ok = false;
    }

    match Config::load() {
        Ok(cfg) => {
            let agent = run_capture(&cfg.agent.command, &["--version"]);
            ok &= check(
                &format!("agent ({})", cfg.agent.command),
                agent.is_ok(),
                agent.unwrap_or_else(|e| e),
            );
            let n = cfg.projects.len();
            println!(
                "{} projects: {n} configured{}",
                if n > 0 { "✅" } else { "⚠️ " },
                if n > 0 {
                    ""
                } else {
                    " — add one to config.toml before running"
                },
            );
            // Auto-merge preconditions (ADR 0003): only for projects that
            // enabled it — the same gate `meguri watch` fail-fasts on.
            ok &= check_auto_merge(&cfg).await;
        }
        Err(e) => {
            ok = check("config", false, format!("{e:#}"));
        }
    }

    if ok {
        println!("\nall good — try `meguri run --issue <N>`");
        Ok(())
    } else {
        bail!("doctor found problems");
    }
}

/// Doctor item: for every project that enabled auto-merge, confirm the
/// repository can honor it (the same validation `meguri watch` fail-fasts on,
/// ADR 0003). Returns false if any enabled project fails; projects that did
/// not enable auto-merge print nothing.
async fn check_auto_merge(cfg: &Config) -> bool {
    use meguri::engine::auto_merger::validate_policy;
    use meguri::forge::Forge;
    use meguri::forge::gh::GhForge;

    let mut ok = true;
    for project in &cfg.projects {
        let am = &cfg.pr_for(project).auto_merge;
        if !am.enabled {
            continue;
        }
        let forge = GhForge::new(&project.repo_slug);
        let label = format!("auto-merge ({})", project.id);
        match forge.merge_policy(&project.default_branch).await {
            Ok(policy) => match validate_policy(am, &policy) {
                Ok(()) => println!(
                    "✅ {label}: repo settings OK (strategy={}, protection {})",
                    am.strategy.as_str(),
                    if policy.protected_with_required_checks {
                        "present"
                    } else {
                        "not required"
                    },
                ),
                Err(problems) => {
                    println!("❌ {label}: {}", problems.join("; "));
                    ok = false;
                }
            },
            Err(e) => {
                println!("❌ {label}: cannot read repo merge settings: {e:#}");
                ok = false;
            }
        }
    }
    ok
}

fn run_capture(cmd: &str, args: &[&str]) -> std::result::Result<String, String> {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("not found ({e})")),
    }
}
