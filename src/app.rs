//! CLI command implementations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, ProjectConfig};
use crate::daemon;
use crate::engine::reaper;
use crate::engine::scheduler::{Reload, Scheduler};
use crate::engine::worker::{WorkerOutcome, run_worker};
use crate::engine::{self, Deps};
use crate::forge::gh::GhForge;
use crate::mux;
use crate::notify::Notifier;
use crate::store::{DesiredState, ROLE_AUTHOR, ROLE_REVIEW, RunRecord, RunStatus, Store};

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

pub async fn cmd_serve(port: Option<u16>, bind: Option<&str>) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let bind = bind.unwrap_or(&cfg.server.bind);
    let port = port.unwrap_or(cfg.server.port);
    let addr: std::net::IpAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address {bind:?}"))?;
    if !addr.is_loopback() {
        eprintln!(
            "⚠️  binding {bind} — the dashboard has no authentication; \
             anyone who can reach this address can read run data"
        );
    }
    let listener = tokio::net::TcpListener::bind((addr, port))
        .await
        .with_context(|| format!("cannot bind {bind}:{port}"))?;
    println!("meguri dashboard on http://{}", listener.local_addr()?);
    crate::server::serve(store, cfg, listener).await
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

pub fn cmd_attach(needle: &str, review: bool) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let (kind, pane) = resolve_attach_pane(&store, needle, review)?;
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

/// Resolve what `meguri attach <needle> [--review]` should attach to. Panes
/// belong to the issue's lanes (author + review, kept until the issue
/// closes), so the issue's persistent lane pane wins over whatever pane id
/// a run once recorded — and a bare issue number keeps working after its
/// runs finished. A run id derives its lane from the run's loop kind;
/// `--review` picks the review lane for issue numbers.
fn resolve_attach_pane(store: &Store, needle: &str, review: bool) -> Result<(String, String)> {
    let wanted_role = if review { ROLE_REVIEW } else { ROLE_AUTHOR };
    if let Some(run) = store.find_run(needle)? {
        let role = engine::role_for_loop(&run.loop_kind);
        if let Some(p) = store.get_pane(&run.project_id, run.issue_number, role)?
            && let (Some(kind), Some(id)) = (p.mux_kind, p.mux_pane_id)
        {
            return Ok((kind, id));
        }
        if let (Some(kind), Some(id)) = (&run.mux_kind, &run.mux_pane_id) {
            return Ok((kind.clone(), id.clone()));
        }
        bail!("run {} has no pane yet", run.id);
    }
    if let Ok(issue) = needle.parse::<i64>() {
        let panes: Vec<_> = store
            .panes_for_issue(issue)?
            .into_iter()
            .filter(|p| p.role == wanted_role)
            .collect();
        match panes.as_slice() {
            [] => {
                if review {
                    bail!("issue #{issue} has no live review pane");
                }
            }
            [p] => {
                if let (Some(kind), Some(id)) = (&p.mux_kind, &p.mux_pane_id) {
                    return Ok((kind.clone(), id.clone()));
                }
            }
            many => {
                let projects: Vec<&str> = many.iter().map(|p| p.project_id.as_str()).collect();
                bail!(
                    "issue #{issue} has {wanted_role} panes in multiple projects ({}) — \
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
                let released = reaper::release_pane(
                    &deps,
                    run.issue_number,
                    engine::role_for_loop(&run.loop_kind),
                    "stopped by user",
                )
                .await;
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
