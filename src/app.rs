//! CLI command implementations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, ProjectConfig, ProjectMode};
use crate::daemon;
use crate::engine::reaper;
use crate::engine::scheduler::{Reload, Scheduler};
use crate::engine::worker::{WorkerOutcome, run_worker};
use crate::engine::{self, Deps};
use crate::forge::Forge;
use crate::forge::gh::GhForge;
use crate::mux;
use crate::notify::Notifier;
use crate::store::{
    DesiredState, DriftRow, LANE_AUTHOR, LANE_PR_REVIEW, RunRecord, RunStatus, Store,
};
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
fn build_coordination(
    cfg: &Config,
    project: &ProjectConfig,
    store: &Store,
) -> Result<Coordination> {
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
                cfg.reconcile,
                project.cadence.clone(),
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
    // Per-project workspace: this project's panes live in `<session>:<project>`
    // (herdr) / `<session>-<project>` (tmux), not the shared base workspace.
    let mux = mux::detect(kind, &cfg.mux.session, Some(&project.id))?;
    let store = open_store()?;
    let (forge, task_source) = build_coordination(cfg, project, &store)?;
    Ok(Deps {
        store,
        mux,
        forge,
        task_source,
        notifier: Arc::new(Notifier::from_config(&cfg.notifications)),
        forge_factory: Arc::new(crate::forge::gh::GhForgeFactory),
        config: cfg.clone(),
        project: project.clone(),
        open_prs: Default::default(),
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
    crate::routing::validate(&cfg, &crate::routing::detect_command)?;
    crate::launch::validate(&cfg)?;
    let project = pick_project(&cfg, project)?;
    if project.mode == ProjectMode::Local {
        bail!(
            "`meguri run --issue` is for github-mode projects; \
             for a local project use `meguri add` and let `meguri watch` pick it up"
        );
    }
    let deps = build_deps(&cfg, project, mux_override)?;

    let gh_issue = deps.forge().get_issue(issue).await?;
    // Manual run bypasses the cadence gate (it is a human's explicit override —
    // always run it), but if the issue falls under a cadence rule the run must
    // still count toward the window, or a same-day `watch` would consume the
    // bucket a second time. Conflicting rules are the one case we refuse: a
    // single `cadence_label` cannot count two buckets, so a human must pick.
    let cadence_label = match crate::cadence::cadence_bucket(&gh_issue.labels, &project.cadence) {
        Ok(bucket) => bucket,
        Err(labels) => bail!(
            "issue #{issue} matches multiple cadence rules ({}); a run can only count \
             toward one — remove all but one of these labels",
            labels.join(", ")
        ),
    };
    let run = match deps.store.create_run_for_loop_cadence(
        &project.id,
        "worker",
        issue,
        &gh_issue.title,
        cadence_label.as_deref(),
    ) {
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
    crate::routing::validate(&cfg, &crate::routing::detect_command)?;
    crate::launch::validate(&cfg)?;
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

    // Auto-merge fail-fast (ADR 0003): if a project enabled auto-merge but its
    // repository can't honor it, refuse to start rather than degrade silently
    // at merge time.
    for deps in &projects {
        auto_merge_preflight(deps).await?;
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

/// Startup fail-fast for one project's auto-merge config (ADR 0003): if
/// enabled, the repository must allow auto-merge, permit the configured
/// strategy, and (when required) carry required-checks branch protection.
/// A miss bails with every reason so the operator fixes them at once.
async fn auto_merge_preflight(deps: &Deps) -> Result<()> {
    let am = &deps.config.pr_for(&deps.project).auto_merge;
    if !am.enabled {
        return Ok(());
    }
    // Auto-merge is a GitHub-PR concern; a forge-less (local-mode) project has
    // no PRs to arm, so there is nothing to fail-fast on.
    let Some(forge) = &deps.forge else {
        return Ok(());
    };
    let slug = deps
        .project
        .repo_slug
        .as_deref()
        .unwrap_or(&deps.project.id);
    let policy = forge
        .merge_policy(&deps.project.default_branch, am.require_branch_protection)
        .await
        .with_context(|| format!("cannot read merge settings for {slug} to validate auto-merge"))?;
    if let Err(problems) = crate::engine::auto_merger::validate_policy(am, &policy) {
        bail!(
            "auto-merge is enabled for project `{}` ({}) but the repository cannot \
             honor it:\n  - {}",
            deps.project.id,
            slug,
            problems.join("\n  - "),
        );
    }
    Ok(())
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

/// `meguri stats routing` — success rate / mean turns / mean duration per
/// `(role, profile)` over the last N scored runs, plus any active drift.
/// Pure sqlite direct-read, so it works with the watch stopped. `project =
/// None` spans every project with a project column; `Some(id)` restricts to one.
pub fn cmd_stats_routing(project: Option<&str>) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let window = cfg.drift.window;

    let rows = store.routing_stats(project, window)?;
    if rows.is_empty() {
        match project {
            Some(p) => println!("no routing stats yet for project {p}"),
            None => println!("no routing stats yet"),
        }
    } else {
        println!("routing stats — last {window} scored run(s) per (role, profile, arm)\n");
        println!(
            "{:<8} {:<18} {:<16} {:<10} {:>5} {:>8} {:>9} {:>9}",
            "PROJECT", "ROLE", "PROFILE", "ARM", "RUNS", "SUCCESS", "AVGTURNS", "AVGDUR"
        );
        for r in &rows {
            let profile = if r.agent_profile.is_empty() {
                "(unrouted)"
            } else {
                &r.agent_profile
            };
            let dur = r
                .avg_duration_secs
                .map(|s| format!("{s:.0}s"))
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<8} {:<18} {:<16} {:<10} {:>5} {:>7.0}% {:>9.1} {:>9}",
                r.project_id,
                r.loop_kind,
                profile,
                r.routing_arm,
                r.runs,
                r.success_rate,
                r.avg_turns,
                dur,
            );
        }
    }

    let drifts = store.active_drift(project)?;
    if !drifts.is_empty() {
        println!("\ndrift (成績が悪化):");
        for d in &drifts {
            println!("  ⚠️  {}", drift_label(d));
        }
    }
    Ok(())
}

/// `meguri add`: queue a local task. Phase 1 only serves local-mode projects
/// (silent mode's `meguri queue --issue` is Phase 2); a task added to a github
/// project would never be discovered, so refuse it loudly instead.
pub fn cmd_add(
    project: Option<&str>,
    plan: bool,
    file: Option<&str>,
    title: Option<&str>,
    not_before: Option<&str>,
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
    // Normalize --not-before to our RFC3339 UTC shape up front, so a typo is
    // caught here rather than silently keeping the task queued forever.
    let not_before = match not_before {
        Some(raw) => {
            let ts = crate::cadence::parse_not_before_value(raw).map_err(|e| {
                anyhow::anyhow!(
                    "invalid --not-before {:?}: expected YYYY-MM-DD or an RFC3339 UTC instant",
                    e.raw
                )
            })?;
            Some(crate::store::format_epoch(ts))
        }
        None => None,
    };
    let (title, body) = resolve_task_input(title, file)?;
    let kind = if plan { TaskKind::Plan } else { TaskKind::Work };
    let store = open_store()?;
    let task = store.create_task_with_not_before(
        &project.id,
        kind.as_str(),
        &title,
        &body,
        "local",
        not_before.as_deref(),
    )?;
    println!(
        "queued task #{} [{}] {}",
        task.id,
        kind.as_str(),
        task.title
    );
    if let Some(nb) = &task.not_before {
        println!("not-before {nb} — held until then, then picked up automatically.");
    } else {
        println!("`meguri watch` will pick it up within one poll interval.");
    }
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

/// `meguri tasks`: inspect a project's discovery queue and why each item is (or
/// is not) running. In local mode it lists the local tasks; in github mode it
/// fetches the `ready`/`plan` issues and shows each one's disposition — the same
/// gate discovery applies (issue #148), so silently-skipped work (not-before /
/// cadence) that leaves no trace on the forge is visible here.
pub async fn cmd_tasks(project: Option<&str>, all: bool) -> Result<()> {
    let cfg = Config::load()?;
    let project = pick_project(&cfg, project)?;
    match project.mode {
        ProjectMode::Local => cmd_tasks_local(project, all),
        ProjectMode::Github => cmd_tasks_github(&cfg, project).await,
    }
}

/// Local-mode listing: the sqlite `tasks`, with a not-before annotation on any
/// still-queued task whose gate has not yet opened.
fn cmd_tasks_local(project: &ProjectConfig, all: bool) -> Result<()> {
    let store = open_store()?;
    let tasks = store.list_tasks(&project.id, all)?;
    if tasks.is_empty() {
        println!("no {}tasks", if all { "" } else { "open " });
        return Ok(());
    }
    let now = crate::engine::scheduler_fire::epoch_now();
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
        if t.status == "queued"
            && let Some(raw) = &t.not_before
        {
            match crate::cadence::parse_not_before_value(raw) {
                Err(_) => println!("        ↳ ⏳ not-before 待ち(解析不能: {raw})"),
                Ok(ts) if crate::cadence::not_before_wait(Some(ts), now).is_some() => {
                    println!(
                        "        ↳ ⏳ not-before 待ち(until {})",
                        crate::store::format_epoch(ts)
                    );
                }
                Ok(_) => {}
            }
        }
    }
    Ok(())
}

/// Github-mode listing: the discovery queue (`ready`/`plan` issues) with each
/// issue's live disposition. Goes through `LabelTaskSource::dispositions`, the
/// same gate pipeline (and per-pass cadence allowance) discovery uses — so what
/// shows `ready` is exactly what discover would run: a second same-bucket issue
/// this pass reads `cadence 待ち`, not `ready`, even when the store count alone
/// is still under the limit.
async fn cmd_tasks_github(cfg: &Config, project: &ProjectConfig) -> Result<()> {
    let store = open_store()?;
    let source = LabelTaskSource::new(
        Arc::new(GhForge::new(project.repo_slug.as_deref().context(
            "github-mode project has no repo_slug (config validation should have caught this)",
        )?)),
        store,
        project.id.clone(),
        cfg.reconcile,
        project.cadence.clone(),
    );

    // ready (worker) and plan (planner) are separate discovery passes, each
    // with its own cadence allowance — mirror that here (an issue rarely carries
    // both trigger labels; dedup by number keeps it listed once if it does).
    let mut rows: Vec<(crate::forge::Issue, crate::cadence::Disposition)> =
        source.dispositions(TaskKind::Work).await?;
    for row in source.dispositions(TaskKind::Plan).await? {
        if !rows.iter().any(|(i, _)| i.number == row.0.number) {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        println!(
            "no {}/{} issues",
            crate::forge::LABEL_READY,
            crate::forge::LABEL_PLAN
        );
        return Ok(());
    }
    rows.sort_by_key(|(i, _)| i.number);

    println!("{:>6}  STATE", "ISSUE");
    for (issue, disposition) in rows {
        println!(
            "{:>6}  {}",
            format!("#{}", issue.number),
            format_disposition(&disposition)
        );
        println!("        {}", issue.title);
    }
    Ok(())
}

/// One-line rendering of a [`crate::cadence::Disposition`] for `meguri tasks`.
fn format_disposition(disposition: &crate::cadence::Disposition) -> String {
    use crate::cadence::Disposition;
    use crate::store::format_epoch;
    match disposition {
        Disposition::Ready => "✅ ready".to_string(),
        Disposition::WaitingNotBefore { until } => {
            format!("⏳ not-before 待ち(until {})", format_epoch(*until))
        }
        Disposition::UnparsableNotBefore { raw } => {
            format!("⏳ not-before 待ち(解析不能: {raw})")
        }
        Disposition::Blocked => "⛔ blocked(未解決の依存)".to_string(),
        Disposition::ConflictingCadenceLabels { labels } => {
            format!("⚠️  cadence ラベル競合({})", labels.join(", "))
        }
        Disposition::WaitingCadence {
            label,
            consumed,
            max,
            resets_at,
        } => {
            let resets = resets_at
                .map(|t| format!(", resets {}", format_epoch(t)))
                .unwrap_or_default();
            format!("⏳ cadence 待ち({label} {consumed}/{max}{resets})")
        }
    }
}

/// `meguri schedules`: list a project's cron schedules with their definition,
/// last fire (from sqlite `schedule_state`), and next fire (computed from the
/// cron expression, UTC). Times are UTC, matching the cron interpretation.
pub fn cmd_schedules(project: Option<&str>) -> Result<()> {
    let cfg = Config::load()?;
    let project = pick_project(&cfg, project)?;
    if project.schedules.is_empty() {
        println!("no schedules configured for {}", project.id);
        return Ok(());
    }
    let store = open_store()?;
    let now = crate::engine::scheduler_fire::epoch_now();
    println!(
        "{:<16} {:<6} {:<16} {:<21} {:<21}",
        "NAME", "KIND", "CRON", "LAST FIRE (UTC)", "NEXT FIRE (UTC)"
    );
    for s in &project.schedules {
        let state = store.get_schedule_state(&project.id, &s.name)?;
        let last = state
            .as_ref()
            .and_then(|st| st.last_fired_at.clone())
            .unwrap_or_else(|| "-".into());
        let next = match crate::cron::Cron::parse(&s.cron) {
            Ok(cron) => cron
                .next_after(now)
                .map(crate::store::format_epoch)
                .unwrap_or_else(|| "never".into()),
            Err(e) => format!("invalid cron: {e}"),
        };
        println!(
            "{:<16} {:<6} {:<16} {:<21} {:<21}",
            s.name,
            s.kind.as_str(),
            s.cron,
            last,
            next
        );
    }
    Ok(())
}

/// A `[project] role/profile` label for a drift row (empty profile = default).
fn drift_label(d: &DriftRow) -> String {
    let profile = if d.agent_profile.is_empty() {
        "default"
    } else {
        &d.agent_profile
    };
    format!("[{}] {}/{}", d.project_id, d.loop_kind, profile)
}

pub fn cmd_ps(all: bool) -> Result<()> {
    let store = open_store()?;
    let runs = store.list_runs(!all)?;
    if runs.is_empty() {
        println!("no {}runs", if all { "" } else { "active " });
        return Ok(());
    }
    // Workspace grouping (issue #154) is display-only and opt-in: with no
    // workspaces configured — or an unreadable config — the listing is exactly
    // as before (acceptance criterion 5). The same config also resolves each
    // run's launch mode (issue #169) — unreadable config falls back to "-"
    // rather than guessing.
    let cfg = Config::load().ok();
    let print_header = || {
        println!(
            "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} {:<14} {:<7} PANE",
            "RUN", "PROJECT", "TARGET", "STATUS", "INTERACTION", "STEP", "PROFILE", "MODE"
        );
    };
    let print_row = |run: &RunRecord| {
        // A github run is keyed by its issue (`#7`), a local run by its task
        // row (`t3`); the branch prefix uses the same convention.
        let target = match run.task_key() {
            crate::tasks::TaskKey::Issue(n) => format!("#{n}"),
            crate::tasks::TaskKey::Local(id) => format!("t{id}"),
        };
        let mode = cfg.as_ref().map_or("-", |c| {
            crate::launch::resolve(c, crate::routing::routing_role_for_loop(&run.loop_kind))
                .as_str()
        });
        println!(
            "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} {:<14} {:<7} {}",
            run.id,
            run.project_id,
            target,
            run.status.as_str(),
            run.interaction_state.map(|s| s.as_str()).unwrap_or("-"),
            run.step,
            run.agent_profile.as_deref().unwrap_or("-"),
            mode,
            run.mux_pane_id.as_deref().unwrap_or("-"),
        );
    };

    let groups = group_by_workspace(cfg.as_ref(), &runs);
    match groups {
        None => {
            print_header();
            for run in &runs {
                print_row(run);
            }
        }
        Some(groups) => {
            for (i, (label, group_runs)) in groups.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("[{label}]");
                print_header();
                for run in group_runs {
                    print_row(run);
                }
            }
        }
    }
    Ok(())
}

/// Group runs by the workspace their project belongs to, for display only
/// (issue #154). Returns `None` when no workspaces are configured (the caller
/// then prints the flat, unchanged listing); otherwise groups in config order,
/// with an "(no workspace)" bucket last for projects that joined none. Empty
/// groups are omitted so a workspace with no active runs prints nothing.
fn group_by_workspace<'a>(
    cfg: Option<&Config>,
    runs: &'a [RunRecord],
) -> Option<Vec<(String, Vec<&'a RunRecord>)>> {
    let cfg = cfg?;
    if cfg.workspaces.is_empty() {
        return None;
    }
    let mut groups: Vec<(String, Vec<&RunRecord>)> = Vec::new();
    for ws in &cfg.workspaces {
        let members: Vec<&RunRecord> = runs
            .iter()
            .filter(|r| ws.projects.iter().any(|p| p == &r.project_id))
            .collect();
        if !members.is_empty() {
            groups.push((format!("workspace: {}", ws.id), members));
        }
    }
    let orphans: Vec<&RunRecord> = runs
        .iter()
        .filter(|r| cfg.workspace_of(&r.project_id).is_none())
        .collect();
    if !orphans.is_empty() {
        groups.push(("no workspace".to_string(), orphans));
    }
    Some(groups)
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
    /// Active routing drift across every project (cross-project view, #65).
    drift: Vec<DriftRow>,
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
/// [`resolve_attach_pane`]: the issue's persistent lane pane (panes table) wins
/// over the pane id a run once recorded, which can be a stale start-of-run
/// snapshot. The lane comes from the run's loop kind (the pr-reviewer keeps
/// its own `pr-review` lane). Returns `(mux_kind, pane_id)`, or `None` when
/// the run has no pane yet.
fn run_pane(store: &Store, run: &RunRecord) -> Result<Option<(String, String)>> {
    let lane = engine::lane_for_loop(&run.loop_kind);
    if let Some(p) = store.get_pane(&run.project_id, run.issue_number, lane)?
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

    // Parked reviews (ADR 0009 / issue #153): review runs that ended Succeeded
    // but still wait on a human (a plan review's findings, or a clean spec PR
    // awaiting a human merge). `list_runs(true)` can't see them (not active),
    // so surface them here as awaiting-human rows. They may have no live pane —
    // the actionable target is the PR, not a pane — so show them regardless,
    // and skip any whose pane an active row already listed.
    for run in store.list_parked_reviews()? {
        let pane_info = run_pane(store, &run)?;
        if let Some((rk, pid)) = &pane_info
            && *rk == kind
            && live_panes.contains(pid)
        {
            continue;
        }
        rows.push(TopRow {
            run_id: run.id.clone(),
            project: run.project_id.clone(),
            issue: run.issue_number,
            interaction: run.interaction_state.map(|s| s.as_str()).unwrap_or("-"),
            agent: mux::AgentState::Unknown.as_str(),
            pane: pane_info
                .map(|(_, id)| id)
                .unwrap_or_else(|| "-".to_string()),
            awaiting_human: true,
        });
    }

    let watch_alive = store
        .latest_heartbeat("watch")?
        .map(|ts| heartbeat_alive(&ts, poll_interval_secs))
        .unwrap_or(false);
    // Cross-project active routing drift (#65), read-only from the state table.
    let drift = store.active_drift(None)?;
    Ok(TopStatus {
        watch_alive,
        rows,
        drift,
    })
}

/// Render the status header printed above the tiled panes each tick. When
/// `cfg` has `[[workspaces]]`, rows are grouped by workspace for display only
/// (issue #154); without workspaces (or config, e.g. tests) the flat listing is
/// unchanged.
fn render_top(status: &TopStatus, attach_hint: &str, cfg: Option<&Config>) -> String {
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
    // Routing drift banner (#65): one cross-project line, only when non-empty.
    if !status.drift.is_empty() {
        let labels: Vec<String> = status.drift.iter().map(drift_label).collect();
        out.push_str(&format!(
            "⚠ routing drift: {} — {}\n",
            status.drift.len(),
            labels.join(", "),
        ));
    }
    let col_header = format!(
        "\n{:<14} {:<8} {:>6}  {:<16} {:<9} PANE\n",
        "RUN", "PROJECT", "ISSUE", "INTERACTION", "AGENT"
    );
    let row_line = |r: &TopRow| {
        // Flag awaiting-human runs so a human eye lands on them first.
        let marker = if r.awaiting_human { "▶ " } else { "  " };
        format!(
            "{marker}{:<12} {:<8} {:>6}  {:<16} {:<9} {}\n",
            r.run_id,
            r.project,
            format!("#{}", r.issue),
            r.interaction,
            r.agent,
            r.pane,
        )
    };

    if status.rows.is_empty() {
        out.push_str("\nno active runs — start one with `meguri watch` or `meguri run`\n");
    } else if let Some(groups) = top_groups(cfg, &status.rows) {
        for (label, rows) in groups {
            out.push_str(&format!("\n[{label}]"));
            out.push_str(&col_header);
            for r in rows {
                out.push_str(&row_line(r));
            }
        }
    } else {
        out.push_str(&col_header);
        for r in &status.rows {
            out.push_str(&row_line(r));
        }
    }
    out.push_str(&format!("\nview tiles: {attach_hint}\n"));
    out
}

/// Group `top` rows by workspace for display (issue #154). `None` when no
/// workspaces are configured (the caller prints the flat listing); otherwise
/// config-ordered groups plus a trailing "no workspace" bucket, empty groups
/// omitted.
fn top_groups<'a>(
    cfg: Option<&Config>,
    rows: &'a [TopRow],
) -> Option<Vec<(String, Vec<&'a TopRow>)>> {
    let cfg = cfg?;
    if cfg.workspaces.is_empty() {
        return None;
    }
    let mut groups: Vec<(String, Vec<&TopRow>)> = Vec::new();
    for ws in &cfg.workspaces {
        let members: Vec<&TopRow> = rows
            .iter()
            .filter(|r| ws.projects.iter().any(|p| p == &r.project))
            .collect();
        if !members.is_empty() {
            groups.push((format!("workspace: {}", ws.id), members));
        }
    }
    let orphans: Vec<&TopRow> = rows
        .iter()
        .filter(|r| cfg.workspace_of(&r.project).is_none())
        .collect();
    if !orphans.is_empty() {
        groups.push(("no workspace".to_string(), orphans));
    }
    Some(groups)
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
    // Cross-project view: build on the base workspace (no project); the
    // dashboard is a separate dedicated workspace/session either way.
    let mux = mux::detect(
        mux_override.unwrap_or(&cfg.mux.kind),
        &cfg.mux.session,
        None,
    )?;
    // The base workspace must exist first (the dashboard is separate).
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
    // `meguri top` is the cross-project view: build on the base workspace (no
    // project) so `tile_pane` can move every project's panes into the dashboard
    // by id, reaching across the per-project workspaces/sessions. The dashboard
    // itself was created by the outer `cmd_top`; here we only tile into it.
    let mux = mux::detect(
        mux_override.unwrap_or(&cfg.mux.kind),
        &cfg.mux.session,
        None,
    )?;
    let dashboard = mux::DashboardId(dashboard.to_string());
    let attach_hint = mux.dashboard_attach_command(&dashboard);

    let interval = Duration::from_secs(interval_secs.max(1));
    let poll = cfg.scheduler.poll_interval_secs;
    let mut tiled: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let status = top_refresh(&store, &mux, &dashboard, &mut tiled, poll).await?;
        print!("{}", render_top(&status, &attach_hint, Some(&cfg)));
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
        // Addresses an existing pane by id; no project-scoped label needed.
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session, None)
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
    // Attach addresses an existing pane by id; the tmux attach command resolves
    // the pane's own session, so no project-scoped label is needed here.
    let mux = mux::from_kind(&kind, &cfg.mux.session, None)?;
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
/// belong to the issue's lanes (author + pr-review, kept until the issue
/// closes), so the issue's persistent lane pane wins over whatever pane id
/// a run once recorded — and a bare issue number keeps working after its
/// runs finished. A run id derives its lane from the run's loop kind;
/// `--review` picks the pr-review lane for issue numbers.
fn resolve_attach_pane(store: &Store, needle: &str, review: bool) -> Result<(String, String)> {
    let wanted_lane = if review { LANE_PR_REVIEW } else { LANE_AUTHOR };
    if let Some(run) = store.find_run(needle)? {
        // `run_pane` derives the run's lane from its loop kind, so a
        // pr-review-lane run resolves its pr-review pane and everything else
        // the author pane — `--review` only matters for the
        // bare-issue-number path below.
        if let Some(pane) = run_pane(store, &run)? {
            return Ok(pane);
        }
        bail!("run {} has no pane yet", run.id);
    }
    if let Ok(issue) = needle.parse::<i64>() {
        let panes: Vec<_> = store
            .panes_for_issue(issue)?
            .into_iter()
            .filter(|p| p.lane == wanted_lane)
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
                    "issue #{issue} has {wanted_lane} panes in multiple projects ({}) — \
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
                    engine::lane_for_loop(&run.loop_kind),
                    "stopped by user",
                )
                .await;
                // Drop the claim through the coordination layer, keyed by
                // whatever this run targets (github: the working label; local:
                // back to queued).
                let _ = deps.task_source.release(&run.task_key()).await;
                released.is_some()
            }
            Err(_) => false,
        },
        None => false,
    };
    if !released
        && let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session, None)
    {
        // Fallback for panes that predate the pane registry. Kills by pane id,
        // so the base label is fine — no project-scoped workspace needed.
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
            drift: vec![],
        };
        let out = render_top(&status, "herdr tab focus wD:t9; herdr", None);
        assert!(out.contains("2 run(s)"));
        assert!(out.contains("1 awaiting human"));
        assert!(out.contains("stale"), "watch liveness must show stale");
        assert!(out.contains("▶ run-bbbb"), "awaiting run gets a marker");
        assert!(out.contains("#42"));
        assert!(out.contains("herdr tab focus wD:t9"));
        assert!(!out.contains("routing drift"), "no drift line when empty");
    }

    #[test]
    fn render_top_shows_routing_drift_line() {
        let status = TopStatus {
            watch_alive: true,
            rows: vec![],
            drift: vec![DriftRow {
                project_id: "demo".into(),
                loop_kind: "worker".into(),
                agent_profile: "claude-sonnet".into(),
                active: true,
                metric_json: "{}".into(),
                detected_at: "2026-07-13T00:00:00Z".into(),
                updated_at: "2026-07-13T00:00:00Z".into(),
            }],
        };
        let out = render_top(&status, "echo attach", None);
        assert!(out.contains("routing drift: 1"));
        assert!(out.contains("[demo] worker/claude-sonnet"));
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
            .upsert_pane(
                "demo",
                7,
                LANE_AUTHOR,
                "tmux",
                "meguri",
                "wD:pN",
                "/wt/demo/7",
            )
            .unwrap();

        // Issue 8: one run, also stale snapshot vs the table's live pane.
        let r3 = store.create_run("demo", 8, "t").unwrap();
        store
            .update_run_mux(&r3.id, "tmux", "meguri", "wD:pStale3")
            .unwrap();
        store
            .upsert_pane(
                "demo",
                8,
                LANE_AUTHOR,
                "tmux",
                "meguri",
                "wD:pR",
                "/wt/demo/8",
            )
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
            drift: vec![],
        };
        let out = render_top(&status, "echo attach", None);
        assert!(out.contains("0 run(s)"));
        assert!(out.contains("no active runs"));
        assert!(out.contains("watch live"));
    }

    fn top_row(project: &str, issue: i64) -> TopRow {
        TopRow {
            run_id: format!("run-{project}-{issue}"),
            project: project.into(),
            issue,
            interaction: "agent_working",
            agent: "working",
            pane: format!("wD:{project}{issue}"),
            awaiting_human: false,
        }
    }

    fn config_with_workspace() -> Config {
        let raw = "\
[[projects]]\nid = \"shop-api\"\nrepo_path = \"/tmp/a\"\nrepo_slug = \"me/a\"\n\
[[projects]]\nid = \"shop-web\"\nrepo_path = \"/tmp/b\"\nrepo_slug = \"me/b\"\n\
[[projects]]\nid = \"loner\"\nrepo_path = \"/tmp/c\"\nrepo_slug = \"me/c\"\n\
[[workspaces]]\nid = \"shop\"\nprojects = [\"shop-api\", \"shop-web\"]\n";
        toml::from_str(raw).unwrap()
    }

    #[test]
    fn render_top_groups_by_workspace_when_configured() {
        let status = TopStatus {
            watch_alive: true,
            rows: vec![
                top_row("shop-api", 1),
                top_row("loner", 2),
                top_row("shop-web", 3),
            ],
            drift: vec![],
        };
        let cfg = config_with_workspace();
        let out = render_top(&status, "echo attach", Some(&cfg));
        // Workspace members grouped under one heading; unworkspaced last.
        assert!(out.contains("[workspace: shop]"), "{out}");
        assert!(out.contains("[no workspace]"), "{out}");
        let ws_at = out.find("[workspace: shop]").unwrap();
        let orphan_at = out.find("[no workspace]").unwrap();
        assert!(ws_at < orphan_at, "workspace group precedes orphans");
        // The unworkspaced project's row sits after the orphan heading.
        assert!(out[orphan_at..].contains("loner"), "{out}");
    }

    #[test]
    fn render_top_stays_flat_without_workspaces() {
        // Acceptance criterion 5: no [[workspaces]] → no grouping headings.
        let status = TopStatus {
            watch_alive: true,
            rows: vec![top_row("demo", 1)],
            drift: vec![],
        };
        let cfg: Config = toml::from_str(
            "[[projects]]\nid = \"demo\"\nrepo_path = \"/tmp/d\"\nrepo_slug = \"me/d\"\n",
        )
        .unwrap();
        let out = render_top(&status, "echo attach", Some(&cfg));
        assert!(!out.contains("[workspace"), "{out}");
        assert!(!out.contains("[no workspace]"), "{out}");
    }

    #[test]
    fn group_by_workspace_omits_empty_groups_and_none_without_config() {
        let cfg = config_with_workspace();
        let store = Store::open_in_memory().unwrap();
        let runs = vec![
            store.create_run("shop-api", 1, "t").unwrap(),
            store.create_run("loner", 2, "t").unwrap(),
        ];
        let groups = group_by_workspace(Some(&cfg), &runs).unwrap();
        // shop has one active member; the empty side is not printed; orphans last.
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "workspace: shop");
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[1].0, "no workspace");
        // No config → flat (None).
        assert!(group_by_workspace(None, &runs).is_none());
    }
}
