use std::io::{IsTerminal, Write};

use anyhow::{Result, bail};
use clap::Parser;
use meguri::agent_skills;
use meguri::app;
use meguri::cli::{AgentSkillsCommand, Cli, Command, DaemonCommand, StatsCommand};
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
        Command::Doctor { probe } => cmd_doctor(probe).await,
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
            not_before,
            title,
        } => app::cmd_add(
            project.as_deref(),
            plan,
            file.as_deref(),
            title.as_deref(),
            not_before.as_deref(),
        ),
        Command::Tasks { project, all } => app::cmd_tasks(project.as_deref(), all).await,
        Command::Schedules { project } => app::cmd_schedules(project.as_deref()),
        Command::Ps { all } => app::cmd_ps(all),
        Command::Stats { command } => match command {
            StatsCommand::Routing { project } => app::cmd_stats_routing(project.as_deref()),
        },
        Command::Top { mux, interval } => app::cmd_top(mux.as_deref(), interval).await,
        Command::TopStatus {
            mux,
            dashboard,
            interval,
        } => app::cmd_top_status(mux.as_deref(), &dashboard, interval).await,
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
        Command::AgentSkills { command } => match command {
            AgentSkillsCommand::Install {
                target,
                project,
                repo,
                force,
            } => app::cmd_agent_skills_install(&target, project, repo.as_deref(), force),
            AgentSkillsCommand::Status {
                target,
                project,
                repo,
            } => app::cmd_agent_skills_status(&target, project, repo.as_deref()),
        },
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
    offer_agent_skills_install();
    Ok(())
}

/// After `meguri init`, offer the user-level Claude Code skill (issue #150)
/// so an agent working nearby can learn about and propose meguri on its
/// own. Interactive only — a non-interactive run (CI, scripted setup) just
/// gets the pointer, never a silent write to `~/.claude/`.
fn offer_agent_skills_install() {
    println!();
    if !std::io::stdin().is_terminal() {
        println!(
            "Tip: `meguri agent-skills install` sets up a Claude Code skill \
             (~/.claude/skills/meguri/) so agents working nearby can learn about meguri."
        );
        return;
    }
    print!("Also install the meguri skill for Claude Code (~/.claude/skills/meguri/)? [y/N] ");
    if std::io::stdout().flush().is_err() {
        return;
    }
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        println!("Skipped — run `meguri agent-skills install` any time.");
        return;
    }
    let home = match agent_skills::resolve_home() {
        Ok(home) => home,
        Err(e) => {
            println!("⚠️  could not install agent skill: {e:#}");
            return;
        }
    };
    match agent_skills::install_user_skill(agent_skills::Target::Claude, &home, false) {
        Ok(report) => app::print_agent_skills_install_report(&report),
        Err(e) => println!("⚠️  could not install agent skill: {e:#}"),
    }
}

async fn cmd_doctor(probe: bool) -> Result<()> {
    let mut ok = true;

    // Best-effort: the DB backs CLI-version drift and routing-drift display.
    // A missing/broken store just means those checks are skipped, not a fail.
    let store = Store::open(&config::db_path()).ok();

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
            ok &= doctor_agents(cfg, store.as_ref(), probe);
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
            // Outcome-based routing drift (routing 2/3, #65): surface any
            // active drift the watch's sweep recorded. Read-only, all-project.
            if let Some(store) = &store {
                doctor_drift(store);
            }
            doctor_workspaces(cfg);
            // Auto-merge preconditions (ADR 0003): only for projects that
            // enabled it — the same gate `meguri watch` fail-fasts on.
            ok &= check_auto_merge(cfg).await;
            // Cron schedules (issue #146): cron/name/body validity already
            // fail-fast at load; here we check body_file existence and show
            // the next fire.
            ok &= doctor_schedules(cfg).await;
            // Cadence rules (issue #148): shape is already validated at load;
            // here we show each rule's current window consumption.
            doctor_cadence(cfg, store.as_ref());
            // Repo config (issue #165): lint each project's `meguri.toml` on the
            // default branch, failing on a host-only key or TOML error.
            ok &= doctor_repo_configs(cfg).await;
            // Role preambles (issue #149): each configured path must resolve to
            // a regular file on the default branch (ADR 0015).
            ok &= doctor_prompts(cfg).await;
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
    use meguri::config::AutoMergeMode;
    use meguri::engine::auto_merger::validate_policy;
    use meguri::forge::Forge;
    use meguri::forge::gh::GhForge;

    let mut ok = true;
    for project in &cfg.projects {
        let am = &cfg.pr_for(project).auto_merge;
        if !am.enabled {
            continue;
        }
        // Inconsistency warn (issue #176): auto-merge is on, but the project is
        // `attended`, so meguri will never arm it. Advisory only — not a failure.
        if cfg.autonomy_for(project) != meguri::config::Autonomy::Full {
            println!(
                "⚠️  auto-merge ({}): enabled but autonomy=attended — meguri will not arm \
                 auto-merge (set autonomy = \"full\" to arm; ADR 0012)",
                project.id
            );
        }
        // Auto-merge is a GitHub-PR concern; a local-mode project has no slug
        // and no PRs to arm, so there is nothing to check.
        let Some(slug) = &project.repo_slug else {
            continue;
        };
        let forge = GhForge::new(slug);
        let label = format!("auto-merge ({})", project.id);
        match forge
            .merge_policy(&project.default_branch, am.require_branch_protection)
            .await
        {
            Ok(policy) => match validate_policy(am, &policy) {
                Ok(()) => match am.mode {
                    AutoMergeMode::Native => println!(
                        "✅ {label}: repo settings OK (mode=native, strategy={}, protection {})",
                        am.strategy.as_str(),
                        if policy.protected_with_required_checks {
                            "present"
                        } else {
                            "not required"
                        },
                    ),
                    AutoMergeMode::Orchestrator => {
                        // No server-side gate exists in this mode — remind the
                        // operator that meguri's own verification is the gate.
                        println!(
                            "✅ {label}: repo settings OK (mode=orchestrator, strategy={})",
                            am.strategy.as_str(),
                        );
                        println!(
                            "   ⚠️  orchestrator mode: no server-side merge gate — \
                             meguri's own check_command + self-review is the only gate"
                        );
                    }
                },
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

/// Doctor's schedules section (issue #146): the cron expression, name
/// uniqueness, body exclusivity, and local-mode `plan` rejection are already
/// enforced at config load (so a loaded `cfg` has passed them). What load does
/// *not* check is that each `body_file` is a regular file on the default branch
/// — do that here (ADR 0015), and print each schedule's next fire. Returns false
/// if any `body_file` is missing/unreadable; projects without schedules print
/// nothing.
async fn doctor_schedules(cfg: &Config) -> bool {
    use meguri::cron::Cron;
    use meguri::gitops::{self, DefaultBranchFile};
    use meguri::store::format_epoch;

    let has_any = cfg.projects.iter().any(|p| !p.schedules.is_empty());
    if !has_any {
        return true;
    }
    let now = meguri::engine::scheduler_fire::epoch_now();
    let mut ok = true;
    println!("\nschedules:");
    for project in &cfg.projects {
        for s in &project.schedules {
            let next = Cron::parse(&s.cron)
                .ok()
                .and_then(|c| c.next_after(now))
                .map(format_epoch)
                .unwrap_or_else(|| "never".into());
            // body_file must be a regular file on the default branch (ADR
            // 0015); inline body is always fine.
            let (line_ok, body_detail) = match &s.body_file {
                Some(rel) => match gitops::read_file_at_default_branch(
                    &project.repo_path,
                    &project.default_branch,
                    rel,
                )
                .await
                {
                    Ok(DefaultBranchFile::Content(_)) => (true, format!("body_file {rel}")),
                    Ok(DefaultBranchFile::Absent) => {
                        (false, format!("body_file {rel} not on default branch"))
                    }
                    Ok(DefaultBranchFile::NotRegularFile) => (
                        false,
                        format!("body_file {rel} is not a regular file on default branch"),
                    ),
                    Err(e) => (false, format!("body_file {rel}: {e:#}")),
                },
                None => (true, "inline body".to_string()),
            };
            ok &= line_ok;
            println!(
                "  {} {}/{} ({} {}, next {next} UTC) — {body_detail}",
                if line_ok { "✅" } else { "❌" },
                project.id,
                s.name,
                s.kind.as_str(),
                s.cron,
            );
        }
    }
    ok
}

/// Doctor's cadence section (issue #148): the config shape (label uniqueness,
/// period mode, positive values) already fail-fasts at load, so here we simply
/// show each rule's current window consumption — "N/M used, K left" — so an
/// operator can see why a labelled issue is being held back. Projects without
/// cadence rules print nothing; a missing store just omits the counts.
fn doctor_cadence(cfg: &Config, store: Option<&Store>) {
    use meguri::cadence;

    let has_any = cfg.projects.iter().any(|p| !p.cadence.is_empty());
    if !has_any {
        return;
    }
    let now = meguri::engine::scheduler_fire::epoch_now();
    println!("\ncadence:");
    for project in &cfg.projects {
        for rule in &project.cadence {
            let mode = match rule.per_hours {
                Some(h) => format!("per {h}h"),
                None => "per day (UTC)".to_string(),
            };
            let max = cadence::limit(rule);
            let usage = match store {
                Some(store) => {
                    let start = cadence::window_start(rule, now);
                    match store.cadence_consumed(&project.id, &rule.label, start) {
                        Ok(consumed) => {
                            let left = (max as i64 - consumed).max(0);
                            format!("{consumed}/{max} used, {left} left")
                        }
                        Err(_) => format!("max {max} (count unavailable)"),
                    }
                }
                None => format!("max {max}"),
            };
            println!("  ✅ {}/{} ({mode}) — {usage}", project.id, rule.label);
        }
    }
}

/// Doctor's repo-config section (issue #165): lint each project's repo root
/// `meguri.toml`. Doctor holds no run, so it reads the default branch's
/// `meguri.toml` (ADR 0015), not the working tree — advisory, not the run's
/// pinned value. A host-only key or malformed TOML fails (deny_unknown_fields);
/// an absent file is the silent, valid opt-out. Follows routing/schedules' "never silently
/// fall back" principle so a boundary violation surfaces here, not as a no-op.
async fn doctor_repo_configs(cfg: &Config) -> bool {
    use meguri::config::RepoConfig;
    use meguri::gitops::{self, DefaultBranchFile};

    let mut printed_header = false;
    let mut ok = true;
    let mut header = || {
        if !printed_header {
            println!("\nrepo config:");
            printed_header = true;
        }
    };
    for project in &cfg.projects {
        // Lint the default branch's `meguri.toml` (ADR 0015), not the working
        // tree. An absent file is the silent, valid opt-out.
        let read = gitops::read_file_at_default_branch(
            &project.repo_path,
            &project.default_branch,
            "meguri.toml",
        )
        .await;
        match read {
            Ok(DefaultBranchFile::Absent) => {}
            Ok(DefaultBranchFile::Content(raw)) => {
                header();
                match RepoConfig::parse_str(&raw) {
                    Ok(_) => println!("  ✅ {}: meguri.toml OK", project.id),
                    Err(e) => {
                        println!("  ❌ {}: {e:#}", project.id);
                        ok = false;
                    }
                }
            }
            Ok(DefaultBranchFile::NotRegularFile) => {
                header();
                println!(
                    "  ❌ {}: meguri.toml is not a regular file on default branch",
                    project.id
                );
                ok = false;
            }
            Err(e) => {
                header();
                println!("  ❌ {}: {e:#}", project.id);
                ok = false;
            }
        }
    }
    ok
}

/// Doctor's preamble section (issue #149): for every project, every preamble
/// path that could be injected for it — the top-level `[prompts]` overlaid by
/// its own `[projects.prompts]` — must be a regular file on the default branch
/// (ADR 0015). Missing paths and non-regular files (a symlink, which can't be
/// followed in a blob read) are both reported as ❌ (config validate already
/// rejects absolute / `..` / trailing-slash values, so those never reach here).
/// Projects with no preambles configured print nothing.
async fn doctor_prompts(cfg: &Config) -> bool {
    use meguri::gitops::{self, DefaultBranchFile};

    let has_any = !cfg.prompts.is_empty() || cfg.projects.iter().any(|p| !p.prompts.is_empty());
    if !has_any {
        return true;
    }
    let mut ok = true;
    println!("\npreambles:");
    for project in &cfg.projects {
        for (key, rel) in cfg.effective_prompts(project) {
            // Verify against the default branch (ADR 0015), reading the blob
            // directly. A symlink can't be followed here, so it is reported as
            // unverifiable rather than silently passing (its target string
            // would otherwise read as content).
            let (mark, detail) = match gitops::read_file_at_default_branch(
                &project.repo_path,
                &project.default_branch,
                &rel,
            )
            .await
            {
                Ok(DefaultBranchFile::Content(_)) => ("✅", rel.to_string()),
                Ok(DefaultBranchFile::Absent) => {
                    ok = false;
                    ("❌", format!("{rel} not on default branch"))
                }
                Ok(DefaultBranchFile::NotRegularFile) => {
                    ok = false;
                    (
                        "❌",
                        format!("{rel} is not a regular file on default branch (symlink/dir)"),
                    )
                }
                Err(e) => {
                    ok = false;
                    ("❌", format!("{rel}: {e:#}"))
                }
            };
            println!("  {mark} {}/{key} — {detail}", project.id);
        }
    }
    ok
}

/// Doctor's workspace section (issue #154): list each `[[workspaces]]` group
/// and its member projects. Reaching here means the config already loaded, and
/// loading hard-fails on an undefined project reference or a project in two
/// workspaces — so a printed workspace is a validated one. An invalid
/// workspace instead surfaces on doctor's `config` line (the load error).
fn doctor_workspaces(cfg: &Config) {
    if cfg.workspaces.is_empty() {
        return;
    }
    println!("\nworkspaces:");
    for ws in &cfg.workspaces {
        println!("  ✅ {} → {}", ws.id, ws.projects.join(", "));
    }
}

/// Doctor's routing section: list every defined profile (default + builtin +
/// user) with its detection result and — with `--probe` — a live model-alias
/// probe, record CLI major-version drift, warn on a stale recommendation
/// table, and print the role→profile resolution. Returns whether the mandatory
/// `default` profile CLI is present, explicit routing validates, and no probe
/// found an invalid model.
fn doctor_agents(cfg: &Config, store: Option<&Store>, probe: bool) -> bool {
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

    // CLI major-version drift is per command, but several profiles can share a
    // command (claude-opus / claude-sonnet → `claude`): check each command once.
    let mut version_checked: std::collections::HashSet<String> = std::collections::HashSet::new();

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
    if let (Ok(v), Some(store)) = (&default_detail, store) {
        doctor_version_drift(store, &cfg.agent.command, v, &mut version_checked);
    }

    let mut ok = default_ok;
    if probe {
        ok &= doctor_probe("default", &cfg.agent, &routing::probe_profile);
    }

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
            detail.clone().unwrap_or_else(|e| e),
        );
        if let (Ok(v), Some(store)) = (&detail, store) {
            doctor_version_drift(store, &profile.command, v, &mut version_checked);
        }
        // Probe only profiles whose CLI was detected — a missing CLI is
        // already reported above and can't be launched.
        if probe && detail.is_ok() {
            ok &= doctor_probe(name, &profile, &routing::probe_profile);
        }
    }

    // Recommendation-table freshness (routing 2/3, #65): warn when the baked-in
    // snapshot has aged past the staleness window.
    match routing::table_age_days() {
        Some(days) if days > routing::TABLE_STALE_DAYS => {
            let ym = routing::GENERATED_AT
                .get(..7)
                .unwrap_or(routing::GENERATED_AT);
            println!(
                "⚠️  routing 推奨は {ym} 版({days} 日前)。新モデルのリリースを確認してください"
            );
        }
        _ => {}
    }

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
    ok &= doctor_launch(cfg);
    ok
}

/// Per-role launch mode (issue #169, ADR 0012): pane vs. direct, always
/// resolved (no legacy/off state, unlike routing). Explicit launch config
/// errors (an unknown role key) are startup errors, surfaced here like
/// routing's.
fn doctor_launch(cfg: &Config) -> bool {
    use meguri::{launch, routing};
    let mut ok = true;
    if let Err(e) = launch::validate(cfg) {
        println!("  ❌ launch config: {e:#}");
        ok = false;
    }
    println!("launch mode:");
    for role in routing::KNOWN_ROLES {
        println!("  {role:<18} → {}", launch::resolve(cfg, role).as_str());
    }
    ok
}

/// Compare a CLI's detected version against the last one doctor recorded and
/// warn on a major-version bump (behavior may have shifted; re-evaluate
/// routing), then persist the current version. Each command is checked once
/// per doctor run.
fn doctor_version_drift(
    store: &Store,
    command: &str,
    version_line: &str,
    checked: &mut std::collections::HashSet<String>,
) {
    use meguri::routing;

    if !checked.insert(command.to_string()) {
        return;
    }
    let major = routing::major_version(version_line);
    match store.get_cli_version(command) {
        Ok(Some((_, Some(prev_major)))) => {
            if let Some(now_major) = major
                && now_major as i64 > prev_major
            {
                println!(
                    "⚠️  {command}: メジャーバージョンが {prev_major} → {now_major} に変化 — \
                     挙動が変わっている可能性。ルーティング再評価を推奨"
                );
            }
        }
        Ok(_) => {} // first sighting: just record below.
        Err(e) => tracing::warn!("cli version read failed for {command}: {e:#}"),
    }
    if let Err(e) = store.record_cli_version(command, version_line, major.map(|m| m as i64)) {
        tracing::warn!("cli version write failed for {command}: {e:#}");
    }
}

/// doctor's severity for one profile's live probe. `ModelInvalid` is fatal (the
/// routing table points at a model that no longer resolves); `Unavailable`
/// (network/auth/unknown CLI) is a non-fatal ⚠️ so a flaky link doesn't fail
/// doctor. Returns whether doctor may still pass.
fn doctor_probe(
    label: &str,
    profile: &config::AgentProfile,
    probe: &dyn Fn(&config::AgentProfile) -> meguri::routing::ProbeOutcome,
) -> bool {
    use meguri::routing::ProbeOutcome;
    let (symbol, detail, fatal) = match probe(profile) {
        ProbeOutcome::Ok => ("✅", "model alias valid".to_string(), false),
        ProbeOutcome::ModelInvalid => (
            "❌",
            format!(
                "model alias rejected by `{}` — routing 表が古い可能性",
                profile.command
            ),
            true,
        ),
        ProbeOutcome::Unavailable => (
            "⚠️ ",
            "probe inconclusive (network/auth/unknown CLI) — doctor は fail させない".to_string(),
            false,
        ),
    };
    println!("  {symbol} probe {label}: {detail}");
    !fatal
}

/// Doctor's routing-drift section: list every project's unresolved outcome
/// drift (recorded by the watch's sweep, routing 2/3 #65). Read-only; a run of
/// doctor never computes drift itself.
fn doctor_drift(store: &Store) {
    let drifts = match store.active_drift(None) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("routing drift read failed: {e:#}");
            return;
        }
    };
    if drifts.is_empty() {
        return;
    }
    println!("\nrouting drift:");
    for d in drifts {
        let profile = if d.agent_profile.is_empty() {
            "default"
        } else {
            &d.agent_profile
        };
        println!(
            "  ⚠️  [{}] {}/{} の成績が悪化 — CLI 更新かモデル変更の影響の可能性",
            d.project_id, d.loop_kind, profile
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use meguri::routing::ProbeOutcome;

    fn fake_profile() -> config::AgentProfile {
        config::AgentProfile {
            command: "fake-claude".into(),
            args: vec![],
            resume_args: vec![],
            direct_args: vec![],
            herdr_agent_hint: None,
            session_dir: None,
        }
    }

    #[test]
    fn probe_invalid_model_fails_doctor() {
        // "fake agent" = injected probe closure: a rejected model is fatal (❌).
        let p = fake_profile();
        let bad = |_: &config::AgentProfile| ProbeOutcome::ModelInvalid;
        assert!(!doctor_probe("default", &p, &bad));
    }

    #[test]
    fn probe_network_failure_does_not_fail_doctor() {
        let p = fake_profile();
        let flaky = |_: &config::AgentProfile| ProbeOutcome::Unavailable;
        assert!(doctor_probe("default", &p, &flaky));
        let ok = |_: &config::AgentProfile| ProbeOutcome::Ok;
        assert!(doctor_probe("default", &p, &ok));
    }

    #[test]
    fn version_drift_warns_only_on_major_bump_and_records() {
        let store = Store::open_in_memory().unwrap();
        let mut checked = std::collections::HashSet::new();

        // First sighting records, no comparison.
        doctor_version_drift(&store, "claude", "claude 1.2.3", &mut checked);
        assert_eq!(
            store.get_cli_version("claude").unwrap(),
            Some(("claude 1.2.3".to_string(), Some(1)))
        );

        // Same command is only checked once per doctor run (dedup within run).
        doctor_version_drift(&store, "claude", "claude 9.9.9", &mut checked);
        assert_eq!(
            store.get_cli_version("claude").unwrap(),
            Some(("claude 1.2.3".to_string(), Some(1))),
            "second call in same run is a no-op"
        );

        // A fresh run (new set) sees the major bump and re-records.
        let mut next_run = std::collections::HashSet::new();
        doctor_version_drift(&store, "claude", "claude 2.0.0", &mut next_run);
        assert_eq!(
            store.get_cli_version("claude").unwrap(),
            Some(("claude 2.0.0".to_string(), Some(2)))
        );
    }
}
