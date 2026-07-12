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
        Command::Doctor => cmd_doctor(),
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
        Command::Run {
            project,
            issue,
            mux,
        } => app::cmd_run(project.as_deref(), issue, mux.as_deref()).await,
        Command::Add {
            project,
            plan,
            file,
            title,
        } => app::cmd_add(project.as_deref(), plan, file.as_deref(), title.as_deref()),
        Command::Tasks { project, all } => app::cmd_tasks(project.as_deref(), all),
        Command::Ps { all } => app::cmd_ps(all),
        Command::Top { mux, interval } => app::cmd_top(mux.as_deref(), interval).await,
        Command::Logs { run } => app::cmd_logs(&run).await,
        Command::Attach { run, review } => app::cmd_attach(&run, review),
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

fn cmd_doctor() -> Result<()> {
    let mut ok = true;

    let check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}: {detail}", if pass { "✅" } else { "❌" });
        pass
    };

    // Whether any configured project talks to GitHub. If every project is
    // local-mode, gh/gh-auth are informational, not required (issue #54).
    // A config that fails to load is treated conservatively as needing gh.
    let cfg = Config::load();
    let needs_github = match &cfg {
        Ok(c) => c
            .projects
            .iter()
            .any(|p| p.mode != config::ProjectMode::Local),
        Err(_) => true,
    };

    let git = run_capture("git", &["--version"]);
    ok &= check("git", git.is_ok(), git.unwrap_or_else(|e| e));

    let gh = run_capture("gh", &["--version"]);
    let gh_present = gh.is_ok();
    let gh_pass = check(
        "gh",
        gh_present,
        gh.map(|v| v.lines().next().unwrap_or_default().to_string())
            .unwrap_or_else(|e| e),
    );
    if gh_present {
        let auth = run_capture("gh", &["auth", "status"]);
        let auth_pass = check(
            "gh auth",
            auth.is_ok(),
            auth.map(|_| "authenticated".into()).unwrap_or_else(|e| e),
        );
        if needs_github {
            ok &= auth_pass;
        }
    }
    if needs_github {
        ok &= gh_pass;
    } else if !gh_pass {
        println!("   (all projects are local-mode — gh is optional)");
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

    match &cfg {
        Ok(cfg) => {
            ok &= doctor_agents(cfg);
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

/// Doctor's routing section: list every defined profile (default + builtin +
/// user) with its detection result, then — when `[routing]` is active — the
/// final role→profile resolution. Returns whether the mandatory `default`
/// profile CLI is present and explicit routing validates.
fn doctor_agents(cfg: &Config) -> bool {
    use meguri::routing;

    // Merged profile set: builtins first, user profiles override same names.
    // `default` (the [agent] section) is listed separately and is mandatory.
    let mut names: Vec<String> = routing::builtin_profiles().into_keys().collect();
    if let Some(agents) = &cfg.agents {
        for name in agents.profiles.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names.sort();

    println!("\nagent profiles:");
    // The default profile is required: a missing CLI here breaks every legacy
    // and fall-through run.
    let default_detail = run_capture(&cfg.agent.command, &["--version"])
        .map(|v| v.lines().next().unwrap_or_default().to_string());
    let default_ok = default_detail.is_ok();
    println!(
        "  {} default ({}): {}",
        if default_ok { "✅" } else { "❌" },
        cfg.agent.command,
        default_detail.clone().unwrap_or_else(|e| e),
    );
    // Named profiles are optional — a missing CLI just prunes it from the auto
    // chain; only report their detection, don't fail doctor.
    for name in &names {
        let profile = routing::profile_by_name(cfg, name).expect("listed profile resolves");
        let detail = run_capture(&profile.command, &["--version"])
            .map(|v| v.lines().next().unwrap_or_default().to_string());
        println!(
            "  {} {name} ({}): {}",
            if detail.is_ok() { "✅" } else { "⚠️ " },
            profile.command,
            detail.unwrap_or_else(|e| e),
        );
    }

    let mut ok = default_ok;
    match &cfg.routing {
        None => {
            println!("routing: legacy — every role runs `default` ([agent])");
        }
        Some(routing_cfg) => {
            let mode = match routing_cfg.mode {
                meguri::config::RoutingMode::Auto => "auto",
                meguri::config::RoutingMode::Manual => "manual",
            };
            // Explicit routing errors are startup errors: surface them here.
            if let Err(e) = routing::validate(cfg, &routing::detect_command) {
                println!("  ❌ routing config: {e:#}");
                ok = false;
            }
            println!("routing ({mode}, table {}):", routing::GENERATED_AT);
            for role in routing::KNOWN_ROLES {
                match routing::resolve(cfg, role, &routing::detect_command) {
                    Ok(profile) => println!("  {role:<18} → {profile}"),
                    Err(e) => {
                        println!("  {role:<18} → ❌ {e:#}");
                        ok = false;
                    }
                }
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
