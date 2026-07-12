//! CLI command implementations.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, ProjectConfig, ProjectMode};
use crate::daemon;
use crate::engine::Deps;
use crate::engine::reaper;
use crate::engine::scheduler::Scheduler;
use crate::engine::worker::{WorkerOutcome, run_worker};
use crate::forge::Forge;
use crate::forge::gh::GhForge;
use crate::mux;
use crate::store::{DesiredState, RunRecord, RunStatus, Store};
use crate::tasks::{LabelTaskSource, LocalTaskSource, TaskKind, TaskSource};

pub fn open_store() -> Result<Store> {
    Store::open(&config::db_path())
}

/// A project's coordination layer: its optional forge (github only) and its
/// task source.
type Coordination = (Option<Arc<dyn Forge>>, Arc<dyn TaskSource>);

/// The coordination layer (and whether there is a forge at all) is chosen by
/// the project mode: labels+GitHub for github, the local sqlite `tasks` table
/// for local. Shared by `build_deps` and the driverless `cmd_stop` finalize.
fn build_coordination(project: &ProjectConfig, store: &Store) -> Result<Coordination> {
    match project.mode {
        ProjectMode::Github => {
            let slug = project.repo_slug.clone().context(
                "github-mode project has no repo_slug (config validation should have caught this)",
            )?;
            let forge: Arc<dyn Forge> = Arc::new(GhForge::new(&slug));
            let ts: Arc<dyn TaskSource> = Arc::new(LabelTaskSource::new(
                forge.clone(),
                store.clone(),
                project.id.clone(),
            ));
            Ok((Some(forge), ts))
        }
        ProjectMode::Local => {
            let ts: Arc<dyn TaskSource> =
                Arc::new(LocalTaskSource::new(store.clone(), project.id.clone()));
            Ok((None, ts))
        }
    }
}

fn build_deps(cfg: &Config, project: &ProjectConfig, mux_override: Option<&str>) -> Result<Deps> {
    let kind = mux_override.unwrap_or(&cfg.mux.kind);
    let mux = mux::detect(kind, &cfg.mux.session)?;
    let store = open_store()?;
    let (forge, task_source) = build_coordination(project, &store)?;
    Ok(Deps {
        store,
        mux,
        forge,
        task_source,
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
    if project.mode == ProjectMode::Local {
        bail!(
            "`meguri run --issue` is for github-mode projects; \
             for a local project use `meguri add` and let `meguri watch` pick it up"
        );
    }
    let deps = build_deps(&cfg, project, mux_override)?;

    let gh_issue = deps.forge().get_issue(issue).await?;
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
    let cfg = Config::load()?;
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
    let scheduler = Scheduler {
        projects,
        loops: crate::engine::default_loops(),
        poll_interval: Duration::from_secs(cfg.scheduler.poll_interval_secs),
        max_concurrent: cfg.scheduler.max_concurrent_runs as usize,
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
        let candidates = reaper::plan(&deps).await?;
        if candidates.is_empty() {
            println!("{}: no meguri worktrees", project.id);
            continue;
        }

        println!("{}:", project.id);
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

/// `meguri add`: queue a local task. Phase 1 only serves local-mode projects
/// (silent mode's `meguri queue --issue` is Phase 2); a task added to a github
/// project would never be discovered, so refuse it loudly instead.
pub fn cmd_add(
    project: Option<&str>,
    plan: bool,
    file: Option<&str>,
    title: Option<&str>,
) -> Result<()> {
    let cfg = Config::load()?;
    let project = pick_project(&cfg, project)?;
    if project.mode != ProjectMode::Local {
        bail!(
            "`meguri add` queues local tasks, but project {:?} is mode = {:?}. \
             Phase 1 supports local mode only (silent mode's `meguri queue --issue` is Phase 2).",
            project.id,
            project.mode.as_str()
        );
    }
    let (title, body) = resolve_task_input(title, file)?;
    let kind = if plan { TaskKind::Plan } else { TaskKind::Work };
    let store = open_store()?;
    let task = store.create_task(&project.id, kind.as_str(), &title, &body, "local")?;
    println!(
        "queued task #{} [{}] {}",
        task.id,
        kind.as_str(),
        task.title
    );
    println!("`meguri watch` will pick it up within one poll interval.");
    Ok(())
}

/// Resolve a task's `(title, body)` from an optional title argument and an
/// optional `--file`. `--file` loads the markdown as the body and, absent an
/// explicit title, lifts the first heading line as the title.
fn resolve_task_input(title: Option<&str>, file: Option<&str>) -> Result<(String, String)> {
    match file {
        Some(path) => {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read task file {path}"))?;
            let title = match title {
                Some(t) => t.to_string(),
                None => first_heading(&body)
                    .context("--file has no heading line; pass a title explicitly")?,
            };
            Ok((title, body))
        }
        None => {
            let title = title
                .context("provide a task title (or --file <path>)")?
                .to_string();
            Ok((title, String::new()))
        }
    }
}

/// The first non-empty line of a markdown document, with leading `#`/spaces
/// stripped — the task title lifted from a `--file`.
fn first_heading(markdown: &str) -> Option<String> {
    let line = markdown.lines().find(|l| !l.trim().is_empty())?;
    Some(line.trim_start_matches('#').trim().to_string())
}

/// `meguri tasks`: list a project's local tasks, newest first. needs_human
/// tasks are highlighted with their reason so a human can pick them up.
pub fn cmd_tasks(project: Option<&str>, all: bool) -> Result<()> {
    let cfg = Config::load()?;
    let project = pick_project(&cfg, project)?;
    let store = open_store()?;
    let tasks = store.list_tasks(&project.id, all)?;
    if tasks.is_empty() {
        println!("no {}tasks", if all { "" } else { "open " });
        return Ok(());
    }
    println!("{:>4}  {:<6} {:<12} TITLE", "ID", "KIND", "STATUS");
    for t in tasks {
        let flag = if t.status == "needs_human" {
            "⚠️ "
        } else {
            ""
        };
        println!(
            "{:>4}  {:<6} {}{:<12} {}",
            t.id, t.kind, flag, t.status, t.title
        );
        if let Some(reason) = t.reason.filter(|_| t.status == "needs_human") {
            println!("        ↳ {reason}");
        }
    }
    Ok(())
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
        "RUN", "PROJECT", "TARGET", "STATUS", "INTERACTION", "STEP"
    );
    for run in runs {
        // A github run is keyed by its issue (`#7`), a local run by its task
        // row (`t3`); the branch prefix uses the same convention.
        let target = match run.task_key() {
            crate::tasks::TaskKey::Issue(n) => format!("#{n}"),
            crate::tasks::TaskKey::Local(id) => format!("t{id}"),
        };
        println!(
            "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} {}",
            run.id,
            run.project_id,
            target,
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

pub fn cmd_attach(needle: &str) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let run = require_run(&store, needle)?;
    let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id) else {
        bail!("run {} has no pane yet", run.id);
    };
    let mux = mux::from_kind(kind, &cfg.mux.session)?;
    let command = mux.attach_command(&mux::PaneId(pane.clone()));
    println!("attaching: {command}");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .exec();
    bail!("exec failed: {err}");
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
    if let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session)
    {
        let _ = mux.kill_pane(&mux::PaneId(pane.clone())).await;
    }
    if let Some(project) = cfg.project(&run.project_id) {
        // Drop the claim (github: the working label; local: back to queued)
        // through the coordination layer, keyed by whatever this run targets.
        let (_forge, task_source) = build_coordination(project, &store)?;
        let _ = task_source.release(&run.task_key()).await;
    }
    store.emit(Some(&run.id), "run.cancelled", serde_json::json!({}))?;
    println!("run {} cancelled", run.id);
    Ok(())
}
