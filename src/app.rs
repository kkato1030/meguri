//! CLI command implementations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, ProjectConfig};
use crate::daemon;
use crate::engine::Deps;
use crate::engine::reaper;
use crate::engine::scheduler::{Reload, Scheduler};
use crate::engine::worker::{WorkerOutcome, run_worker};
use crate::forge::gh::GhForge;
use crate::mux;
use crate::notify::Notifier;
use crate::store::{DesiredState, RunRecord, RunStatus, Store};

pub fn open_store() -> Result<Store> {
    Store::open(&config::db_path())
}

fn build_deps(cfg: &Config, project: &ProjectConfig, mux_override: Option<&str>) -> Result<Deps> {
    let kind = mux_override.unwrap_or(&cfg.mux.kind);
    let mux = mux::detect(kind, &cfg.mux.session)?;
    Ok(Deps {
        store: open_store()?,
        mux,
        forge: Arc::new(GhForge::new(&project.repo_slug)),
        notifier: Arc::new(Notifier::from_config(&cfg.notifications)),
        config: cfg.clone(),
        project: project.clone(),
    })
}

fn pick_project<'a>(cfg: &'a Config, id: Option<&str>) -> Result<&'a ProjectConfig> {
    match id {
        Some(id) => cfg
            .project(id)
            .with_context(|| format!("project {id:?} not in config")),
        None => match cfg.projects.as_slice() {
            [] => bail!(
                "no projects configured — edit {}",
                config::config_path().display()
            ),
            [only] => Ok(only),
            _ => bail!("multiple projects configured — pass --project <id>"),
        },
    }
}

pub async fn cmd_run(project: Option<&str>, issue: i64, mux_override: Option<&str>) -> Result<()> {
    let cfg = Config::load()?;
    let project = pick_project(&cfg, project)?;
    let deps = build_deps(&cfg, project, mux_override)?;

    let gh_issue = deps.forge.get_issue(issue).await?;
    let run = match deps.store.create_run(&project.id, issue, &gh_issue.title) {
        Ok(run) => run,
        Err(_) => {
            // An active run exists (possibly interrupted): resume it.
            let existing = deps
                .store
                .list_runs(true)?
                .into_iter()
                .find(|r| r.project_id == project.id && r.issue_number == issue)
                .context("an active run exists but could not be loaded")?;
            println!("resuming run {} (step {})", existing.id, existing.step);
            existing
        }
    };

    println!(
        "run {} — issue #{} {:?} — watch with: meguri attach {}",
        run.id, issue, gh_issue.title, run.id
    );
    match run_worker(&deps, &run.id).await? {
        WorkerOutcome::Succeeded { pr_url } => {
            println!("✅ PR: {pr_url}");
            Ok(())
        }
        WorkerOutcome::Stopped => {
            println!("🛑 stopped");
            Ok(())
        }
        WorkerOutcome::Interrupted(reason) => {
            bail!("run interrupted: {reason} — rerun `meguri run --issue {issue}` to resume");
        }
        WorkerOutcome::Skipped(reason) => {
            println!("⏭️  skipped: {reason}");
            Ok(())
        }
        WorkerOutcome::NeedsPlan(reason) => {
            println!(
                "📝 needs a plan first — issue handed to {}: {reason}",
                crate::forge::LABEL_PLAN
            );
            Ok(())
        }
        WorkerOutcome::Decomposed(reason) => {
            println!("🧩 decomposed into sub-issues: {reason}");
            Ok(())
        }
    }
}

pub async fn cmd_watch() -> Result<()> {
    let mut reloader = config::ConfigReloader::load(&config::config_path())?;
    let cfg = reloader.current().clone();
    if cfg.projects.is_empty() {
        bail!(
            "no projects configured — edit {}",
            config::config_path().display()
        );
    }

    // Single-instance guard: held for the watch's whole lifetime, so a second
    // scheduler (foreground or detached) fails loudly instead of double-driving.
    let home = config::meguri_home();
    let _lock = daemon::try_acquire_lock(&home)?;
    let mode = daemon::WatchMode::from_env();
    daemon::write_state(
        &home,
        &daemon::DaemonState::for_current_process(&home, mode),
    )?;

    let mut projects = Vec::new();
    for project in &cfg.projects {
        projects.push(build_deps(&cfg, project, None)?);
    }
    println!(
        "watching {} project(s) for {}/{} issues and {}/{} PRs (poll {}s, slots {})",
        projects.len(),
        crate::forge::LABEL_READY,
        crate::forge::LABEL_PLAN,
        crate::forge::LABEL_SPEC_REVIEWING,
        crate::forge::LABEL_SPEC_READY,
        cfg.scheduler.poll_interval_secs,
        cfg.scheduler.max_concurrent_runs,
    );

    // Hot reload (issue #73): every tick re-reads config.toml, so edits reach
    // the runs spawned after them without a daemon restart. Notifiers carry
    // per-run throttle state across turn boundaries, so each project keeps
    // its notifier through a reload unless [notifications] itself changed.
    let mut notifiers: HashMap<String, Arc<Notifier>> = projects
        .iter()
        .map(|d| (d.project.id.clone(), d.notifier.clone()))
        .collect();
    let reload = Box::new(move || {
        let next = reloader.poll(|prev, next| {
            let keep_notifiers = next.notifications == prev.notifications;
            let mut fresh = Vec::new();
            for project in &next.projects {
                let mut deps = build_deps(next, project, None)?;
                if keep_notifiers && let Some(notifier) = notifiers.get(&project.id) {
                    deps.notifier = notifier.clone();
                }
                fresh.push(deps);
            }
            Ok(Reload {
                projects: fresh,
                poll_interval: Duration::from_secs(next.scheduler.poll_interval_secs),
                max_concurrent: next.scheduler.max_concurrent_runs as usize,
            })
        })?;
        notifiers = next
            .projects
            .iter()
            .map(|d| (d.project.id.clone(), d.notifier.clone()))
            .collect();
        Some(next)
    });

    let scheduler = Scheduler {
        projects,
        loops: crate::engine::default_loops(),
        poll_interval: Duration::from_secs(cfg.scheduler.poll_interval_secs),
        max_concurrent: cfg.scheduler.max_concurrent_runs as usize,
        reload: Some(reload),
    };
    let result = scheduler.watch().await;
    daemon::clear_state(&home);
    result
}

pub async fn cmd_prune(project: Option<&str>, dry_run: bool, force: bool) -> Result<()> {
    let cfg = Config::load()?;
    let projects: Vec<&ProjectConfig> = match project {
        Some(id) => vec![pick_project(&cfg, Some(id))?],
        None => cfg.projects.iter().collect(),
    };
    if projects.is_empty() {
        bail!(
            "no projects configured — edit {}",
            config::config_path().display()
        );
    }

    for project in projects {
        let deps = build_deps(&cfg, project, None)?;
        let mut states = reaper::IssueStates::default();
        let pane_candidates = reaper::plan_panes(&deps, &mut states).await?;

        // Panes go first so their worktrees become reclaimable in this same
        // pass (a closed issue's live pane no longer protects its worktree).
        if !pane_candidates.is_empty() {
            println!("{}:", project.id);
            println!("  {:<9} {:<18} PANE", "ISSUE", "STATE");
            for c in &pane_candidates {
                let state = match c.verdict {
                    reaper::Verdict::Reclaim => "reclaim".to_string(),
                    other => format!("{} (skip)", other.as_str()),
                };
                println!("  {:<9} {:<18} {}", format!("#{}", c.issue), state, c.pane);
            }
        }
        if !dry_run {
            let reclaimed = reaper::reclaim_panes(&deps, &pane_candidates).await?;
            if !reclaimed.is_empty() {
                println!("  reclaimed {} pane(s)", reclaimed.len());
                for p in &reclaimed {
                    if let Some(id) = &p.agent_session_id {
                        println!("  saved session for #{}: claude --resume {id}", p.issue);
                    }
                }
            }
        }

        let candidates = reaper::plan_with(&deps, &mut states).await?;
        if candidates.is_empty() {
            if pane_candidates.is_empty() {
                println!("{}: no meguri panes or worktrees", project.id);
            }
            continue;
        }

        if pane_candidates.is_empty() {
            println!("{}:", project.id);
        }
        println!("  {:<9} {:<18} {:>9}  PATH", "ISSUE", "STATE", "SIZE");
        for c in &candidates {
            let state = match c.verdict {
                reaper::Verdict::Reclaim => "reclaim".to_string(),
                reaper::Verdict::Dirty if force => "reclaim (dirty)".to_string(),
                reaper::Verdict::Dirty => "dirty (skip)".to_string(),
                other => format!("{} (skip)", other.as_str()),
            };
            println!(
                "  {:<9} {:<18} {:>9}  {}",
                c.issue
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| "-".into()),
                state,
                human_size(reaper::dir_size(&c.path)),
                c.path.display(),
            );
        }
        if dry_run {
            continue;
        }

        let reclaimed = reaper::reclaim(&deps, &candidates, force).await?;
        let dirty_skipped = candidates
            .iter()
            .filter(|c| c.verdict == reaper::Verdict::Dirty)
            .count();
        println!("  reclaimed {} worktree(s)", reclaimed.len());
        for r in &reclaimed {
            if !r.branch_deleted
                && let Some(branch) = &r.branch
            {
                println!("  kept branch {branch} (not merged; delete with --force)");
            }
        }
        if !force && dirty_skipped > 0 {
            println!("  skipped {dirty_skipped} dirty worktree(s) — rerun with --force to reclaim");
        }
    }
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn require_run(store: &Store, needle: &str) -> Result<RunRecord> {
    store
        .find_run(needle)?
        .with_context(|| format!("no run matches {needle:?} (try `meguri ps --all`)"))
}

pub fn cmd_ps(all: bool) -> Result<()> {
    let store = open_store()?;
    let runs = store.list_runs(!all)?;
    if runs.is_empty() {
        println!("no {}runs", if all { "" } else { "active " });
        return Ok(());
    }
    println!(
        "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} PANE",
        "RUN", "PROJECT", "ISSUE", "STATUS", "INTERACTION", "STEP"
    );
    for run in runs {
        println!(
            "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} {}",
            run.id,
            run.project_id,
            format!("#{}", run.issue_number),
            run.status.as_str(),
            run.interaction_state.map(|s| s.as_str()).unwrap_or("-"),
            run.step,
            run.mux_pane_id.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// Label of the dedicated dashboard workspace/session `meguri top` builds.
/// Derived from the configured session so it stays distinct from the agent
/// workspace and aligns with future per-project workspace separation.
fn top_label(session: &str) -> String {
    format!("{session}:top")
}

/// One row of the `meguri top` status header — one active run and its pane.
struct TopRow {
    run_id: String,
    project: String,
    issue: i64,
    interaction: &'static str,
    agent: &'static str,
    pane: String,
    awaiting_human: bool,
}

/// A rendered snapshot of the dashboard for one refresh tick.
struct TopStatus {
    watch_alive: bool,
    rows: Vec<TopRow>,
}

/// Freshness window for the watch heartbeat: two poll ticks plus slack, so a
/// single slow tick doesn't flap the liveness indicator. Mirrors the retired
/// `serve` dashboard (ADR 0002) — the heartbeat's only reader now.
fn heartbeat_alive(ts: &str, poll_interval_secs: u64) -> bool {
    let Some(then) = crate::store::parse_ts(ts) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    now.saturating_sub(then) < poll_interval_secs * 2 + 30
}

/// Resolve the pane an active run drives, following the same precedence as
/// [`resolve_attach_pane`]: the issue's persistent pane (panes table) wins over
/// the pane id a run once recorded, which can be a stale start-of-run snapshot.
/// Returns `(mux_kind, pane_id)`, or `None` when the run has no pane yet.
fn run_pane(store: &Store, run: &RunRecord) -> Result<Option<(String, String)>> {
    if let Some(p) = store.get_pane(&run.project_id, run.issue_number)?
        && let (Some(kind), Some(id)) = (p.mux_kind, p.mux_pane_id)
    {
        return Ok(Some((kind, id)));
    }
    if let (Some(kind), Some(id)) = (&run.mux_kind, &run.mux_pane_id) {
        return Ok(Some((kind.clone(), id.clone())));
    }
    Ok(None)
}

/// One refresh: tile any newly-appeared live run panes into the dashboard and
/// collect the status rows. `tiled` remembers panes already moved so each is
/// joined exactly once; dead/finished panes are pruned so a reused pane id
/// re-tiles cleanly. Runs on a different mux than the one we drive are skipped.
/// Panes are resolved via the panes table ([`run_pane`]), not the run's
/// start-of-run snapshot, so stale ids no longer read as all-`unknown`.
async fn top_refresh(
    store: &Store,
    mux: &Arc<dyn mux::Multiplexer>,
    dashboard: &mux::DashboardId,
    tiled: &mut std::collections::HashSet<String>,
    poll_interval_secs: u64,
) -> Result<TopStatus> {
    let runs = store.list_runs(true)?;
    let kind = mux.kind().as_str();
    let mut rows = Vec::new();
    let mut live_panes: std::collections::HashSet<String> = std::collections::HashSet::new();

    for run in &runs {
        let Some((rk, pid)) = run_pane(store, run)? else {
            continue;
        };
        if rk != kind {
            continue;
        }
        // 1 issue = 1 pane: several active runs on the same issue resolve to
        // the same pane, so dedup keeps one row and tiles it once.
        if !live_panes.insert(pid.clone()) {
            continue;
        }
        let pane = mux::PaneId(pid.clone());
        let alive = mux.pane_alive(&pane).await.unwrap_or(false);
        if alive
            && !tiled.contains(&pid)
            && mux
                .tile_pane(&pane, dashboard, mux::Split::Down)
                .await
                .is_ok()
        {
            tiled.insert(pid.clone());
        }
        let agent = if alive {
            mux.agent_state(&pane)
                .await
                .unwrap_or(mux::AgentState::Unknown)
        } else {
            mux::AgentState::Unknown
        };
        let awaiting_human =
            run.interaction_state == Some(crate::store::InteractionState::AwaitingHuman);
        rows.push(TopRow {
            run_id: run.id.clone(),
            project: run.project_id.clone(),
            issue: run.issue_number,
            interaction: run.interaction_state.map(|s| s.as_str()).unwrap_or("-"),
            agent: agent.as_str(),
            pane: pid.clone(),
            awaiting_human,
        });
    }

    // Forget panes whose run is no longer active: herdr/tmux reflow on close,
    // and a later reuse of the id must tile again.
    tiled.retain(|id| live_panes.contains(id));

    let watch_alive = store
        .latest_heartbeat("watch")?
        .map(|ts| heartbeat_alive(&ts, poll_interval_secs))
        .unwrap_or(false);
    Ok(TopStatus { watch_alive, rows })
}

/// Render the status header printed above the tiled panes each tick.
fn render_top(status: &TopStatus, attach_hint: &str) -> String {
    let awaiting = status.rows.iter().filter(|r| r.awaiting_human).count();
    let mut out = String::new();
    // Clear screen + home cursor so the header refreshes in place.
    out.push_str("\x1b[2J\x1b[H");
    out.push_str(&format!(
        "meguri top — {} run(s) · {} awaiting human · watch {}\n",
        status.rows.len(),
        awaiting,
        if status.watch_alive {
            "live"
        } else {
            "stale ⚠"
        },
    ));
    if status.rows.is_empty() {
        out.push_str("\nno active runs — start one with `meguri watch` or `meguri run`\n");
    } else {
        out.push_str(&format!(
            "\n{:<14} {:<8} {:>6}  {:<16} {:<9} PANE\n",
            "RUN", "PROJECT", "ISSUE", "INTERACTION", "AGENT"
        ));
        for r in &status.rows {
            // Flag awaiting-human runs so a human eye lands on them first.
            let marker = if r.awaiting_human { "▶ " } else { "  " };
            out.push_str(&format!(
                "{marker}{:<12} {:<8} {:>6}  {:<16} {:<9} {}\n",
                r.run_id,
                r.project,
                format!("#{}", r.issue),
                r.interaction,
                r.agent,
                r.pane,
            ));
        }
    }
    out.push_str(&format!("\nview tiles: {attach_hint}\n"));
    out
}

/// argv of the internal status-render loop (`meguri top-status`) that runs
/// inside the dashboard's status pane, pinned to the same mux and dashboard the
/// outer `meguri top` set up.
fn top_status_argv(
    kind: &str,
    dashboard: &mux::DashboardId,
    interval_secs: u64,
) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("locating the meguri binary")?;
    Ok(vec![
        exe.to_string_lossy().into_owned(),
        "top-status".into(),
        "--mux".into(),
        kind.into(),
        "--dashboard".into(),
        dashboard.0.clone(),
        "--interval".into(),
        interval_secs.to_string(),
    ])
}

/// `meguri top` — build (once) a dedicated dashboard workspace/session holding
/// a status pane plus the tiled live agent panes, then `exec`-attach the caller
/// to it so the screen is immediately visible. The status-render loop lives in
/// the status pane (`meguri top-status`), not this process; here we only set it
/// up and hand the terminal over — mirroring `cmd_attach`. The layout only
/// moves panes between containers, so the orchestrator keeps driving each pane
/// by id and the watch loop continues uninterrupted.
pub async fn cmd_top(mux_override: Option<&str>, interval_secs: u64) -> Result<()> {
    let cfg = Config::load()?;
    let mux = mux::detect(mux_override.unwrap_or(&cfg.mux.kind), &cfg.mux.session)?;
    // The agent workspace must exist first (the dashboard is separate).
    mux.ensure_session().await?;
    let label = top_label(&cfg.mux.session);
    let dashboard = mux.ensure_dashboard(&label).await?;
    // Start the render loop only on a fresh dashboard, so re-running `meguri
    // top` just re-attaches instead of double-driving the header.
    if dashboard.fresh
        && let Some(status_pane) = &dashboard.status_pane
    {
        let argv = top_status_argv(mux.kind().as_str(), &dashboard.tile, interval_secs)?;
        mux.run_in_pane(status_pane, &argv).await?;
    }

    let attach = mux.dashboard_attach_command(&dashboard.tile);
    println!("attaching: {attach}");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("sh")
        .arg("-c")
        .arg(&attach)
        .exec();
    bail!("exec failed: {err}");
}

/// `meguri top-status` (internal) — the render loop that runs inside a
/// dashboard's status pane: tile any newly live agent panes into `dashboard`
/// and refresh the status header in place on its own terminal. Not for humans
/// (hidden subcommand); `meguri top` launches it.
pub async fn cmd_top_status(
    mux_override: Option<&str>,
    dashboard: &str,
    interval_secs: u64,
) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let mux = mux::detect(mux_override.unwrap_or(&cfg.mux.kind), &cfg.mux.session)?;
    let dashboard = mux::DashboardId(dashboard.to_string());
    let attach_hint = mux.dashboard_attach_command(&dashboard);

    let interval = Duration::from_secs(interval_secs.max(1));
    let poll = cfg.scheduler.poll_interval_secs;
    let mut tiled: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let status = top_refresh(&store, &mux, &dashboard, &mut tiled, poll).await?;
        print!("{}", render_top(&status, &attach_hint));
        use std::io::Write;
        let _ = std::io::stdout().flush();
        tokio::time::sleep(interval).await;
    }
}

pub async fn cmd_logs(needle: &str) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let run = require_run(&store, needle)?;

    for event in store.events_for_run(&run.id, 100)? {
        println!("{} {:<24} {}", event.ts, event.kind, event.data);
    }

    if let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session)
    {
        let pane = mux::PaneId(pane.clone());
        if mux.pane_alive(&pane).await.unwrap_or(false) {
            println!("\n--- pane tail ({kind} {pane}) ---");
            for line in mux.read_tail(&pane, 25).await.unwrap_or_default() {
                println!("{line}");
            }
            println!("--- attach: {} ---", mux.attach_command(&pane));
        }
    }
    Ok(())
}

pub fn cmd_attach(needle: &str) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let (kind, pane) = resolve_attach_pane(&store, needle)?;
    let mux = mux::from_kind(&kind, &cfg.mux.session)?;
    let command = mux.attach_command(&mux::PaneId(pane));
    println!("attaching: {command}");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .exec();
    bail!("exec failed: {err}");
}

/// Resolve what `meguri attach <needle>` should attach to. The pane belongs
/// to the issue (1 issue = 1 pane, kept until the issue closes), so the
/// issue's persistent pane wins over whatever pane id a run once recorded —
/// and a bare issue number keeps working after its runs finished.
fn resolve_attach_pane(store: &Store, needle: &str) -> Result<(String, String)> {
    if let Some(run) = store.find_run(needle)? {
        if let Some(pane) = run_pane(store, &run)? {
            return Ok(pane);
        }
        bail!("run {} has no pane yet", run.id);
    }
    if let Ok(issue) = needle.parse::<i64>() {
        let panes = store.panes_for_issue(issue)?;
        match panes.as_slice() {
            [] => {}
            [p] => {
                if let (Some(kind), Some(id)) = (&p.mux_kind, &p.mux_pane_id) {
                    return Ok((kind.clone(), id.clone()));
                }
            }
            many => {
                let projects: Vec<&str> = many.iter().map(|p| p.project_id.as_str()).collect();
                bail!(
                    "issue #{issue} has panes in multiple projects ({}) — \
                     pass a run id instead",
                    projects.join(", ")
                );
            }
        }
    }
    bail!("no run or pane matches {needle:?} (try `meguri ps --all`)")
}

fn set_desired(needle: &str, desired: Option<DesiredState>, verb: &str) -> Result<()> {
    let store = open_store()?;
    let run = require_run(&store, needle)?;
    if !run.status.is_active() {
        bail!("run {} is {}; cannot {verb}", run.id, run.status.as_str());
    }
    store.set_desired_state(&run.id, desired)?;
    store.emit(
        Some(&run.id),
        "control.requested",
        serde_json::json!({ "verb": verb }),
    )?;
    println!("{verb} requested for {}", run.id);
    Ok(())
}

pub fn cmd_pause(needle: &str) -> Result<()> {
    set_desired(needle, Some(DesiredState::Paused), "pause")
}

pub fn cmd_resume(needle: &str) -> Result<()> {
    set_desired(needle, None, "resume")
}

pub fn cmd_takeover(needle: &str) -> Result<()> {
    let out = set_desired(needle, Some(DesiredState::Takeover), "takeover");
    if out.is_ok() {
        println!("the orchestrator is hands-off; `meguri attach` and drive the agent.");
        println!("hand control back with: meguri handback <run>");
    }
    out
}

pub fn cmd_handback(needle: &str) -> Result<()> {
    set_desired(needle, None, "handback")
}

pub async fn cmd_stop(needle: &str) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let run = require_run(&store, needle)?;
    if !run.status.is_active() {
        bail!("run {} is already {}", run.id, run.status.as_str());
    }
    store.set_desired_state(&run.id, Some(DesiredState::Stopped))?;

    if run.status == RunStatus::Running {
        // A live driver will observe desired=stopped within a poll tick and
        // finalize (labels, pane, status) itself.
        println!(
            "stop requested for {}; the orchestrator will finalize it",
            run.id
        );
        return Ok(());
    }

    // No driver is running this (queued/interrupted): finalize here.
    store.update_run_status(&run.id, RunStatus::Cancelled, Some("stopped by user"))?;
    let released = match cfg.project(&run.project_id) {
        Some(project) => match build_deps(&cfg, project, None) {
            Ok(deps) => {
                // Session id is saved before the kill — resumable later.
                let released =
                    reaper::release_pane(&deps, run.issue_number, "stopped by user").await;
                let _ = deps
                    .forge
                    .remove_label(run.issue_number, crate::forge::LABEL_WORKING)
                    .await;
                released.is_some()
            }
            Err(_) => false,
        },
        None => false,
    };
    if !released
        && let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session)
    {
        // Fallback for panes that predate the pane registry.
        let _ = mux.kill_pane(&mux::PaneId(pane.clone())).await;
    }
    store.emit(Some(&run.id), "run.cancelled", serde_json::json!({}))?;
    println!("run {} cancelled", run.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now;

    #[test]
    fn heartbeat_freshness_window() {
        // Fresh beat within two poll ticks + slack reads live.
        assert!(heartbeat_alive(&now(), 60));
        // Ancient and unparseable both read stale, never live.
        assert!(!heartbeat_alive("2000-01-01T00:00:00Z", 60));
        assert!(!heartbeat_alive("garbage", 60));
    }

    #[test]
    fn render_top_flags_awaiting_and_watch_liveness() {
        let status = TopStatus {
            watch_alive: false,
            rows: vec![
                TopRow {
                    run_id: "run-aaaa".into(),
                    project: "demo".into(),
                    issue: 42,
                    interaction: "agent_working",
                    agent: "working",
                    pane: "wD:p1".into(),
                    awaiting_human: false,
                },
                TopRow {
                    run_id: "run-bbbb".into(),
                    project: "demo".into(),
                    issue: 43,
                    interaction: "awaiting_human",
                    agent: "blocked",
                    pane: "wD:p2".into(),
                    awaiting_human: true,
                },
            ],
        };
        let out = render_top(&status, "herdr tab focus wD:t9; herdr");
        assert!(out.contains("2 run(s)"));
        assert!(out.contains("1 awaiting human"));
        assert!(out.contains("stale"), "watch liveness must show stale");
        assert!(out.contains("▶ run-bbbb"), "awaiting run gets a marker");
        assert!(out.contains("#42"));
        assert!(out.contains("herdr tab focus wD:t9"));
    }

    #[tokio::test]
    async fn top_refresh_resolves_panes_from_table_and_dedups() {
        use crate::mux::fake::FakeMux;

        let store = Store::open_in_memory().unwrap();
        let fake = Arc::new(FakeMux::new(true)); // kind() == tmux
        let mux: Arc<dyn mux::Multiplexer> = fake.clone();

        // The panes table holds the *live* ids; the runs carry stale snapshots.
        fake.register_live_pane("wD:pN");
        fake.register_live_pane("wD:pR");

        // Issue 7: two active runs share one pane (worker + fixer).
        let r1 = store.create_run("demo", 7, "t").unwrap();
        store
            .update_run_mux(&r1.id, "tmux", "meguri", "wD:pStale1")
            .unwrap();
        let r2 = store.create_run_for_loop("demo", "fixer", 7, "t").unwrap();
        store
            .update_run_mux(&r2.id, "tmux", "meguri", "wD:pStale2")
            .unwrap();
        store
            .upsert_pane("demo", 7, "tmux", "meguri", "wD:pN", "/wt/demo/7")
            .unwrap();

        // Issue 8: one run, also stale snapshot vs the table's live pane.
        let r3 = store.create_run("demo", 8, "t").unwrap();
        store
            .update_run_mux(&r3.id, "tmux", "meguri", "wD:pStale3")
            .unwrap();
        store
            .upsert_pane("demo", 8, "tmux", "meguri", "wD:pR", "/wt/demo/8")
            .unwrap();

        let dashboard = mux::DashboardId("dash".into());
        let mut tiled = std::collections::HashSet::new();
        let status = top_refresh(&store, &mux, &dashboard, &mut tiled, 60)
            .await
            .unwrap();

        // One row per pane (issue 7's two runs collapse), from the panes table.
        let mut panes: Vec<&str> = status.rows.iter().map(|r| r.pane.as_str()).collect();
        panes.sort_unstable();
        assert_eq!(panes, vec!["wD:pN", "wD:pR"]);
        // The stale run snapshots are never touched.
        assert!(!panes.iter().any(|p| p.contains("Stale")));
        // Live panes read as working, not unknown (the #104 regression).
        assert!(status.rows.iter().all(|r| r.agent == "working"));

        // Each live pane tiled exactly once, by its table id.
        let mut tiled_ids: Vec<String> = fake
            .tiled_panes()
            .into_iter()
            .map(|(p, _, _)| p.0)
            .collect();
        tiled_ids.sort_unstable();
        assert_eq!(tiled_ids, vec!["wD:pN".to_string(), "wD:pR".to_string()]);
    }

    #[test]
    fn render_top_handles_no_runs() {
        let status = TopStatus {
            watch_alive: true,
            rows: vec![],
        };
        let out = render_top(&status, "echo attach");
        assert!(out.contains("0 run(s)"));
        assert!(out.contains("no active runs"));
        assert!(out.contains("watch live"));
    }
}
