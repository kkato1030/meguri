//! The cleaner loop: a read-only detector that periodically sweeps the
//! repository at the default branch head and rewrites a single per-project
//! report issue (`meguri:clean-report`) with the divergence it found —
//! spec/implementation drift, dead-code candidates, convention violations,
//! stranded TODOs, stale remote branches, orphaned `meguri:working` labels.
//!
//! Its write boundary is exactly that one issue (ADR 0003): no pushes, no
//! branch operations, no labels or comments anywhere else — not even a
//! `meguri:working` claim (the run-uniqueness index and the head marker are
//! dedup enough). Humans triage the report: real findings become regular
//! issues for the existing loops, false positives go on the `clean.ignore`
//! list, and `meguri:hold` on the report issue pauses the sweep.
//!
//! Lifetime (issue #92): standalone — keyed by the report issue (whose
//! author lane no other loop ever touches), read-only detached worktree,
//! and self-reclaiming: the report issue never closes, so the cleaner
//! releases its own pane and worktree at the end of every sweep instead of
//! leaving them to the reaper.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, STEP_EXECUTE, STEP_PREPARE_WORK, STEP_PREPARE_WORKTREE};
use super::{Deps, Target};
use crate::config::CleanConfig;
use crate::forge;
use crate::gitops;
use crate::store::{LANE_AUTHOR, RunRecord, RunStatus};
use crate::tasks::TaskKey;
use crate::turn::{TurnOutcome, TurnStatus};

/// `runs.loop_kind` value for cleaner runs.
pub const KIND: &str = "cleaner";

/// Terminal cleaner step: machine checks, render, write the report issue.
pub const STEP_SETTLE: &str = "settle";

/// Where the agent writes its findings (worktree-relative; `.meguri/` is
/// git-excluded, so it never dirties the read-only checkout).
pub const REPORT_FILE: &str = ".meguri/clean-report.json";

/// Title of the per-project report issue.
pub const REPORT_TITLE: &str = "🔁 meguri clean report";

/// Marker `head` value before any sweep completed.
pub const MARKER_HEAD_NONE: &str = "none";

/// Hidden marker embedded in the report issue body: which head the report
/// covers and when the last sweep (or failed attempt) ran. The issue body is
/// the durable scan state — nothing is kept locally ("Authority").
pub fn clean_marker(head: &str, scanned: u64) -> String {
    format!("<!-- meguri:clean head={head} scanned={scanned} -->")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanMarker {
    pub head: String,
    /// Unix epoch seconds of the last sweep attempt.
    pub scanned: u64,
}

pub fn parse_clean_marker(body: &str) -> Option<CleanMarker> {
    let rest = body.split("<!-- meguri:clean ").nth(1)?;
    let fields = rest.split("-->").next()?;
    let mut head = None;
    let mut scanned = None;
    for part in fields.split_whitespace() {
        if let Some(v) = part.strip_prefix("head=") {
            head = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("scanned=") {
            scanned = v.parse().ok();
        }
    }
    Some(CleanMarker {
        head: head?,
        scanned: scanned?,
    })
}

/// The discovery decision, pure so it unit-tests without a forge: sweep when
/// nothing was ever recorded, or when the head moved *and* the configured
/// interval elapsed. The same head is never swept twice, however old.
pub fn needs_scan(
    marker: Option<&CleanMarker>,
    current_head: &str,
    now: u64,
    interval_secs: u64,
) -> bool {
    match marker {
        None => true,
        Some(m) if m.head == current_head => false,
        Some(m) => now.saturating_sub(m.scanned) >= interval_secs,
    }
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    SpecDrift,
    DeadCode,
    Convention,
    Todo,
}

impl Category {
    const ALL: [Category; 4] = [
        Category::SpecDrift,
        Category::DeadCode,
        Category::Convention,
        Category::Todo,
    ];

    fn heading(&self) -> &'static str {
        match self {
            Self::SpecDrift => "Spec drift",
            Self::DeadCode => "Dead-code candidates",
            Self::Convention => "Convention violations",
            Self::Todo => "Stranded TODO / FIXME",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// One agent-detected divergence, as written to [`REPORT_FILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub category: Category,
    pub file: String,
    #[serde(default)]
    pub line: Option<i64>,
    pub note: String,
    pub confidence: Confidence,
}

/// What the agent writes to [`REPORT_FILE`].
#[derive(Debug, Deserialize)]
pub struct ReportFile {
    pub findings: Vec<Finding>,
}

/// A remote branch the deterministic check flagged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleBranch {
    pub name: String,
    pub age_days: u64,
    pub merged: bool,
}

/// A `meguri:working` label with no active run behind it on this host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanWorking {
    pub number: i64,
    pub is_pr: bool,
    pub title: String,
}

/// An open issue that violates the ADR 0005 phase-label invariant (a
/// meguri-engaged open issue carries exactly one phase label): either it has
/// two or more phase labels (a swap that dropped the old one), or it has a
/// ball label (`working` / `needs-human`) with no phase label at all (engaged
/// but its phase went missing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseLabelAnomaly {
    pub number: i64,
    pub title: String,
    /// The phase labels found on the issue (0, or 2+ for an anomaly).
    pub phases: Vec<String>,
    /// Whether a ball label is present (only meaningful when `phases` is empty).
    pub has_ball: bool,
}

/// Results of the settle-step machine checks (no agent involved).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MachineFindings {
    pub stale_branches: Vec<StaleBranch>,
    pub orphan_working: Vec<OrphanWorking>,
    #[serde(default)]
    pub phase_label_anomaly: Vec<PhaseLabelAnomaly>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CleanCheckpoint {
    /// Report issue number; 0 until settle creates it on the first sweep.
    #[serde(default)]
    pub report_issue: i64,
    /// Default-branch head this run sweeps.
    #[serde(default)]
    pub head_sha: String,
    /// Marker head found at claim time (kept on a skipped sweep so the head
    /// is not recorded as scanned).
    #[serde(default)]
    pub prev_head: String,
    /// Verified agent findings, carried from execute to settle.
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// The cleaner as a schedulable loop: one report issue per project in, the
/// same report issue (rewritten) out.
pub struct CleanerLoop;

#[async_trait]
impl super::Loop for CleanerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// One target at most: the project's report issue (or `0` when it does
    /// not exist yet — settle creates it; discovery itself stays read-only).
    /// Not `issue_has_succeeded_run`-guarded for the same reason as the
    /// reviewer: the head marker is the dedup, succeeded sweeps must not
    /// block future ones.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.forge.is_none() {
            return Ok(Vec::new()); // forge-driven loop; inert in local mode
        }
        let issues = deps
            .forge()
            .list_issues_with_label(forge::LABEL_CLEAN_REPORT)
            .await?;
        // More than one report issue (races, manual creation): the smallest
        // number wins, the rest are ignored.
        let report = issues.into_iter().min_by_key(|i| i.number);
        if let Some(issue) = &report
            && issue.has_label(forge::LABEL_HOLD)
        {
            return Ok(Vec::new());
        }
        let head =
            gitops::default_branch_head(&deps.project.repo_path, &deps.project.default_branch)
                .await?;
        let marker = report.as_ref().and_then(|i| parse_clean_marker(&i.body));
        let interval = deps.config.clean_for(&deps.project).interval_hours * 3600;
        if !needs_scan(marker.as_ref(), &head, epoch_now(), interval) {
            return Ok(Vec::new());
        }
        Ok(vec![Target {
            key: TaskKey::Issue(report.map(|i| i.number).unwrap_or(0)),
            title: REPORT_TITLE.to_string(),
            cadence_label: None,
        }])
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_cleaner(deps, run_id).await
    }
}

pub async fn run_cleaner(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    let run = deps
        .store
        .get_run(run_id)?
        .with_context(|| format!("run {run_id} not found"))?;

    deps.store
        .update_run_status(run_id, RunStatus::Running, None)?;
    deps.store.emit(
        Some(run_id),
        "run.started",
        json!({ "issue": run.issue_number, "step": run.step }),
    )?;

    match drive(deps, &run).await {
        Ok(outcome) => {
            match &outcome {
                WorkerOutcome::Succeeded { pr_url } => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Succeeded, None)?;
                    deps.store
                        .emit(Some(run_id), "run.succeeded", json!({ "pr": pr_url }))?;
                }
                WorkerOutcome::Stopped => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Cancelled, None)?;
                    // Go through the shared release path (session save +
                    // mark_pane_reclaimed), like flow::finalize_cancelled —
                    // not a raw kill_pane off run.mux_pane_id, which used to
                    // leave the pane row dangling until the next dead-pane
                    // sweep (issue #169; under the recommended `direct`
                    // launch mode for cleaner this is a no-op, no live pane).
                    super::reaper::release_pane(
                        deps,
                        run.issue_number,
                        LANE_AUTHOR,
                        "stopped by user",
                    )
                    .await;
                    deps.store.emit(Some(run_id), "run.cancelled", json!({}))?;
                }
                WorkerOutcome::Interrupted(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Interrupted, Some(reason))?;
                    deps.store.emit(
                        Some(run_id),
                        "run.interrupted",
                        json!({ "reason": reason }),
                    )?;
                }
                WorkerOutcome::Skipped(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Skipped, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.skipped", json!({ "reason": reason }))?;
                }
                // The cleaner's drive never hands work to the planner; these
                // are unreachable here but recorded faithfully if they occur.
                WorkerOutcome::NeedsPlan(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::NeedsPlan, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.needs_plan", json!({ "reason": reason }))?;
                }
                WorkerOutcome::Decomposed(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Decomposed, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.decomposed", json!({ "reason": reason }))?;
                }
            }
            Ok(outcome)
        }
        Err(e) => {
            // No forge escalation: the write boundary is the report issue
            // alone, and a read-only sweep loses nothing by waiting for the
            // next poll.
            let msg = format!("{e:#}");
            deps.store
                .update_run_status(run_id, RunStatus::Failed, Some(&msg))?;
            deps.store
                .emit(Some(run_id), "run.failed", json!({ "error": msg }))?;
            Err(e)
        }
    }
}

async fn drive(deps: &Deps, run: &RunRecord) -> Result<WorkerOutcome> {
    let mut cp: CleanCheckpoint = serde_json::from_str(&run.checkpoint_json).unwrap_or_default();
    let mut step = run.step.clone();

    if step == STEP_PREPARE_WORK {
        match prepare_work(deps, run, &mut cp).await? {
            Prepared::Ready => {}
            Prepared::Skip(reason) => return Ok(WorkerOutcome::Skipped(reason)),
        }
        step = save_step(deps, run, STEP_PREPARE_WORKTREE, &cp)?;
    }

    if step == STEP_PREPARE_WORKTREE {
        prepare_worktree(deps, run, &cp).await?;
        step = save_step(deps, run, STEP_EXECUTE, &cp)?;
    }

    // Re-read: prepare_worktree persisted branch/worktree_path.
    let run = deps
        .store
        .get_run(&run.id)?
        .context("run vanished mid-drive")?;
    let worktree = PathBuf::from(
        run.worktree_path
            .clone()
            .context("run has no worktree path")?,
    );

    if step == STEP_EXECUTE {
        match execute(deps, &run, &mut cp, &worktree).await? {
            ExecuteFlow::Verified => {}
            ExecuteFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            ExecuteFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
            ExecuteFlow::GiveUp(reason) => {
                // Quiet skip (no needs-human, no comment) — but bump the
                // marker's `scanned` so retries are paced by the interval
                // instead of the poll, then reclaim the pane and worktree (D9).
                settle_skip(deps, &run, &cp).await;
                super::reaper::release_pane(
                    deps,
                    run.issue_number,
                    LANE_AUTHOR,
                    "cleaner sweep gave up",
                )
                .await;
                remove_worktree_best_effort(deps, &run, &worktree).await;
                return Ok(WorkerOutcome::Skipped(reason));
            }
        }
        step = save_step(deps, &run, STEP_SETTLE, &cp)?;
    }

    if step == STEP_SETTLE {
        let issue = settle(deps, &run, &cp).await?;
        // The report issue never closes, so the reaper would keep this
        // pane and detached worktree forever — the cleaner reclaims them
        // itself (D9).
        super::reaper::release_pane(
            deps,
            run.issue_number,
            LANE_AUTHOR,
            "cleaner sweep finished",
        )
        .await;
        remove_worktree_best_effort(deps, &run, &worktree).await;
        return Ok(WorkerOutcome::Succeeded {
            pr_url: format!("issue #{issue}"),
        });
    }

    bail!("unknown step {step:?}");
}

fn save_step(deps: &Deps, run: &RunRecord, step: &str, cp: &CleanCheckpoint) -> Result<String> {
    deps.store
        .update_run_step(&run.id, step, &serde_json::to_string(cp)?)?;
    Ok(step.to_string())
}

async fn remove_worktree_best_effort(deps: &Deps, run: &RunRecord, worktree: &Path) {
    if let Err(e) = gitops::remove_worktree(&deps.project.repo_path, worktree).await {
        tracing::warn!(
            "cannot remove cleaner worktree {}: {e:#}",
            worktree.display()
        );
        return;
    }
    let _ = deps.store.emit(
        Some(&run.id),
        "worktree.reclaimed",
        json!({ "path": worktree.to_string_lossy() }),
    );
}

enum Prepared {
    Ready,
    Skip(String),
}

/// prepare-work: re-verify the sweep is still due (hold, marker, interval)
/// and pin the head this run covers. Deliberately no `meguri:working` claim —
/// the write boundary is the report issue alone (ADR 0003); the run-index
/// and the head marker already dedup.
async fn prepare_work(deps: &Deps, run: &RunRecord, cp: &mut CleanCheckpoint) -> Result<Prepared> {
    let marker = if run.issue_number == 0 {
        // First sweep: if a report issue appeared since discovery (another
        // host, a manual one), defer to it — the next discovery targets its
        // real number and the unique run index applies.
        let issues = deps
            .forge()
            .list_issues_with_label(forge::LABEL_CLEAN_REPORT)
            .await?;
        if let Some(issue) = issues.into_iter().min_by_key(|i| i.number) {
            return Ok(Prepared::Skip(format!(
                "report issue #{} appeared since discovery",
                issue.number
            )));
        }
        None
    } else {
        let issue = deps.forge().get_issue(run.issue_number).await?;
        if issue.has_label(forge::LABEL_HOLD) {
            return Ok(Prepared::Skip(format!(
                "report issue #{} is on hold ({})",
                issue.number,
                forge::LABEL_HOLD
            )));
        }
        parse_clean_marker(&issue.body)
    };

    let head =
        gitops::default_branch_head(&deps.project.repo_path, &deps.project.default_branch).await?;
    let interval = deps.config.clean_for(&deps.project).interval_hours * 3600;
    if !needs_scan(marker.as_ref(), &head, epoch_now(), interval) {
        return Ok(Prepared::Skip(format!(
            "head {head} needs no sweep (already scanned or within interval)"
        )));
    }

    cp.report_issue = run.issue_number;
    cp.head_sha = head;
    cp.prev_head = marker.map(|m| m.head).unwrap_or_default();
    deps.store.emit(
        Some(&run.id),
        "clean.claimed",
        json!({ "issue": cp.report_issue, "head": cp.head_sha }),
    )?;
    Ok(Prepared::Ready)
}

/// prepare-worktree: read-only detached checkout of the default branch head
/// (same mechanism as the reviewer's PR-head checkout).
async fn prepare_worktree(deps: &Deps, run: &RunRecord, cp: &CleanCheckpoint) -> Result<()> {
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let dir = format!("clean-{}", run.id);
    let wt = gitops::worktree_path(&root, &deps.project.id, &dir);
    gitops::create_review_worktree(
        &deps.project.repo_path,
        &wt,
        &deps.project.default_branch,
        &cp.head_sha,
        &deps.project.worktree_setup.exclude,
    )
    .await?;
    deps.store
        .update_run_worktree(&run.id, &deps.project.default_branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.created",
        json!({ "head": cp.head_sha, "path": wt.to_string_lossy() }),
    )?;
    flow::run_worktree_setup(deps, run, &wt).await
}

fn execute_prompt(cp: &CleanCheckpoint, language: Option<&str>) -> String {
    format!(
        "You are sweeping this repository for accumulated divergence. The \
         worktree is a read-only checkout of the default branch head \
         (commit `{sha}`).\n\n\
         # Instructions\n\
         - Walk the repository and report what has drifted, in these \
           categories:\n\
           - `spec-drift`: mismatches between specs (e.g. `docs/specs/`, \
             READMEs, ADRs) and the implementation, or specs that look \
             obsolete.\n\
           - `dead-code`: files or code that appear unused or superseded.\n\
           - `convention`: violations of the repository's own conventions \
             (naming, structure, things the README promises).\n\
           - `todo`: stranded TODO / FIXME comments that look abandoned.\n\
         - Do NOT modify, commit, or push anything; the report file below is \
           your only deliverable.\n\
         - Write your findings to `{report}` as JSON:\n\
           `{{\"findings\": [{{\"category\": \"spec-drift\" | \"dead-code\" | \
           \"convention\" | \"todo\", \"file\": \"src/x.rs\", \"line\": 12, \
           \"note\": \"<what diverged and why you think so>\", \
           \"confidence\": \"high\" | \"medium\" | \"low\"}}]}}`\n\
           - `line` may be null when a finding concerns a whole file.\n\
           - `file` is the repo-relative path the finding is anchored to.\n\
           - Rate `confidence` honestly; a human triages this report.\n\
         - Finding nothing is a valid result: write `{{\"findings\": []}}`.\n\
         - A completed sweep is a success regardless of what it found; report \
           \"failure\"/\"needs_human\" only when you cannot sweep at all.\
         {lang_section}",
        sha = cp.head_sha,
        report = REPORT_FILE,
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

/// The cleaner's deliverable, verified after each turn: a parseable report
/// file and an untouched checkout. The Err text feeds a corrective prompt.
fn read_report(worktree: &Path) -> std::result::Result<ReportFile, String> {
    let raw = std::fs::read_to_string(worktree.join(REPORT_FILE)).map_err(|_| {
        format!("- report file `{REPORT_FILE}` does not exist (write it as instructed)")
    })?;
    serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- report file `{REPORT_FILE}` is not valid JSON ({e}); expected \
             {{\"findings\": [{{\"category\", \"file\", \"line\", \"note\", \
             \"confidence\"}}]}}"
        )
    })
}

enum ExecuteFlow {
    Verified,
    Stopped,
    Interrupted(String),
    /// The agent could not produce a verifiable report (even after the
    /// corrective turn) — give up quietly; the next interval retries.
    GiveUp(String),
}

/// execute: one sweep turn plus at most one corrective turn. Unlike the other
/// loops, a persistently failing agent is NOT escalated — nothing is lost on
/// a read-only sweep, so the run gives up quietly (acceptance criterion).
async fn execute(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut CleanCheckpoint,
    worktree: &Path,
) -> Result<ExecuteFlow> {
    let mut prompt = execute_prompt(cp, deps.config.language_for(&deps.project));
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = flow::run_turn(deps, run, worktree, "clean", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(ExecuteFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(ExecuteFlow::Interrupted("pane died during sweep".into()));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            // needs_plan is a worker signal and decompose a planner one; on
            // a read-only sweep they make no sense — give up like failure.
            TurnStatus::Failure
            | TurnStatus::NeedsHuman
            | TurnStatus::NeedsPlan
            | TurnStatus::Decompose => {
                return Ok(ExecuteFlow::GiveUp(format!(
                    "agent could not complete the sweep: {}",
                    result.summary
                )));
            }
        }

        // Trust but verify: the checkout must be pristine and still at the
        // claimed head, and the report file must parse.
        let clean = gitops::status_clean(worktree).await?;
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        let problem = if !clean || head_now != cp.head_sha {
            Some(format!(
                "- the sweep checkout must stay untouched: working tree clean: \
                 {clean} (must be true), HEAD: {head_now} (must be {expected}) — \
                 discard all changes (`git checkout -- . && git clean -fd && \
                 git reset --hard {expected}`), the report file under .meguri/ \
                 is exempt",
                expected = cp.head_sha,
            ))
        } else {
            read_report(worktree).err()
        };
        let Some(problem) = problem else {
            let report = read_report(worktree).expect("verified above");
            cp.findings = report.findings;
            deps.store.emit(
                Some(&run.id),
                "clean.verified",
                json!({ "findings": cp.findings.len(), "head": cp.head_sha }),
            )?;
            return Ok(ExecuteFlow::Verified);
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Ok(ExecuteFlow::GiveUp(format!(
                "sweep doesn't verify after a corrective turn:\n{problem}"
            )));
        }
        deps.store.emit(
            Some(&run.id),
            "execute.correction",
            json!({ "problem": problem }),
        )?;
        prompt = format!(
            "Your previous result claimed success, but verification failed:\n{problem}\n\n\
             Fix this. Remember: do not modify the checkout; write your report \
             to `{REPORT_FILE}` as instructed.",
        );
    }
}

/// Quiet-skip bookkeeping: advance only the marker's `scanned` (the head is
/// NOT recorded as swept), creating an "initializing" report issue when none
/// exists yet. Without this, a permanently failing agent would burn a turn
/// every poll instead of every interval. Best-effort: a failed write just
/// means the next poll retries sooner.
async fn settle_skip(deps: &Deps, run: &RunRecord, cp: &CleanCheckpoint) {
    let prev_head = if cp.prev_head.is_empty() {
        MARKER_HEAD_NONE
    } else {
        &cp.prev_head
    };
    let marker = clean_marker(prev_head, epoch_now());
    if cp.report_issue == 0 {
        let body = format!(
            "{marker}\n🔁 **meguri clean report** — initializing: the first \
             sweep did not complete. meguri retries after the configured \
             interval (`clean.interval_hours`)."
        );
        match deps
            .forge()
            .create_issue(REPORT_TITLE, &body, &[forge::LABEL_CLEAN_REPORT])
            .await
        {
            Ok(number) => {
                let _ = deps.store.emit(
                    Some(&run.id),
                    "clean.report_created",
                    json!({ "issue": number, "initializing": true }),
                );
                deps.notify_created_issue(number, REPORT_TITLE, &[forge::LABEL_CLEAN_REPORT])
                    .await;
            }
            Err(e) => tracing::warn!("cannot create initializing report issue: {e:#}"),
        }
        return;
    }
    let body = match deps.forge().get_issue(cp.report_issue).await {
        Ok(issue) => replace_marker(&issue.body, &marker),
        Err(e) => {
            tracing::warn!("cannot re-read report issue #{}: {e:#}", cp.report_issue);
            return;
        }
    };
    if let Err(e) = deps.forge().update_issue_body(cp.report_issue, &body).await {
        tracing::warn!("cannot update report issue #{}: {e:#}", cp.report_issue);
    }
}

/// Swap the marker line inside an existing body (or prepend one).
fn replace_marker(body: &str, marker: &str) -> String {
    let Some(start) = body.find("<!-- meguri:clean ") else {
        return format!("{marker}\n{body}");
    };
    let Some(len) = body[start..].find("-->").map(|i| i + 3) else {
        return format!("{marker}\n{body}");
    };
    format!("{}{}{}", &body[..start], marker, &body[start + len..])
}

/// settle: run the deterministic checks, render the snapshot body, and write
/// the report issue — the loop's only forge write. Returns the issue number.
async fn settle(deps: &Deps, run: &RunRecord, cp: &CleanCheckpoint) -> Result<i64> {
    let clean_cfg = deps.config.clean_for(&deps.project).clone();
    let machine = machine_findings(deps, &cp.head_sha, &clean_cfg).await;
    let body = render_report(
        &cp.head_sha,
        epoch_now(),
        &crate::store::now(),
        &cp.findings,
        &machine,
        &clean_cfg.ignore,
    );

    let issue = if cp.report_issue == 0 {
        let number = deps
            .forge()
            .create_issue(REPORT_TITLE, &body, &[forge::LABEL_CLEAN_REPORT])
            .await?;
        deps.store.emit(
            Some(&run.id),
            "clean.report_created",
            json!({ "issue": number }),
        )?;
        deps.notify_created_issue(number, REPORT_TITLE, &[forge::LABEL_CLEAN_REPORT])
            .await;
        number
    } else {
        deps.forge()
            .update_issue_body(cp.report_issue, &body)
            .await?;
        cp.report_issue
    };
    deps.store.emit(
        Some(&run.id),
        "clean.reported",
        json!({
            "issue": issue,
            "head": cp.head_sha,
            "agent_findings": cp.findings.len(),
            "stale_branches": machine.stale_branches.len(),
            "orphan_working": machine.orphan_working.len(),
        }),
    )?;
    Ok(issue)
}

/// The deterministic checks (no agent): stale remote branches and orphaned
/// `meguri:working` labels. Each half degrades to empty on error — a failed
/// check must not sink the agent's findings.
async fn machine_findings(deps: &Deps, head_sha: &str, cfg: &CleanConfig) -> MachineFindings {
    let mut machine = MachineFindings::default();

    match stale_branches(deps, head_sha, cfg.stale_branch_days).await {
        Ok(stale) => machine.stale_branches = stale,
        Err(e) => tracing::warn!("stale-branch check failed: {e:#}"),
    }
    match orphan_working(deps).await {
        Ok(orphans) => machine.orphan_working = orphans,
        Err(e) => tracing::warn!("orphan-working check failed: {e:#}"),
    }
    match phase_label_anomaly(deps).await {
        Ok(anomalies) => machine.phase_label_anomaly = anomalies,
        Err(e) => tracing::warn!("phase-label anomaly check failed: {e:#}"),
    }
    machine
}

/// The four phase labels (ADR 0005, axis 1). A meguri-engaged open issue must
/// carry exactly one of them.
const PHASE_LABELS: [&str; 4] = [
    forge::LABEL_PLAN,
    forge::LABEL_SPECCING,
    forge::LABEL_READY,
    forge::LABEL_IMPLEMENTING,
];

/// The ball labels (ADR 0005, axis 2) that imply meguri is engaged, so a
/// phase label should be present.
const BALL_LABELS: [&str; 2] = [forge::LABEL_WORKING, forge::LABEL_NEEDS_HUMAN];

/// Open issues violating the ADR 0005 phase-label invariant: two or more phase
/// labels (a swap that dropped the old one), or a ball label with no phase
/// label (engaged but the phase went missing). Report-only, like every cleaner
/// check (write boundary = the report issue, ADR 0003). Unlike `orphan_working`
/// this is NOT an "active run behind it" check: phase labels are not claim
/// markers — they live until the issue closes — so the orphan logic would
/// always misfire; the invariant itself is inspected instead. `hold` is a ball
/// label like the others (`hold` alone with no phase reads as "engaged but
/// phase missing"), so it is deliberately excluded — a human-held issue with no
/// phase is a legitimate pause, not a swap bug.
async fn phase_label_anomaly(deps: &Deps) -> Result<Vec<PhaseLabelAnomaly>> {
    // Gather every open issue carrying any phase or ball label. Each returned
    // Issue carries its full label set, so one pass per label suffices to
    // classify it — and the ball-label passes are what surface an issue that
    // has a ball but zero phase labels (it appears under no phase query).
    let mut seen: std::collections::HashMap<i64, forge::Issue> = std::collections::HashMap::new();
    for label in PHASE_LABELS.iter().chain(BALL_LABELS.iter()) {
        for issue in deps.forge().list_issues_with_label(label).await? {
            seen.entry(issue.number).or_insert(issue);
        }
    }

    let mut anomalies: Vec<PhaseLabelAnomaly> = seen
        .into_values()
        .filter_map(|issue| {
            let phases: Vec<String> = PHASE_LABELS
                .iter()
                .filter(|p| issue.has_label(p))
                .map(|p| p.to_string())
                .collect();
            let has_ball = BALL_LABELS.iter().any(|b| issue.has_label(b));
            let anomalous = phases.len() >= 2 || (phases.is_empty() && has_ball);
            anomalous.then_some(PhaseLabelAnomaly {
                number: issue.number,
                title: issue.title,
                phases,
                has_ball,
            })
        })
        .collect();
    // Deterministic order for a stable report body across sweeps.
    anomalies.sort_by_key(|a| a.number);
    Ok(anomalies)
}

/// Remote branches that are merged into the swept head or whose last commit
/// is older than the threshold; the default branch and open-PR heads are
/// exempt (they are alive by definition).
async fn stale_branches(deps: &Deps, head_sha: &str, stale_days: u64) -> Result<Vec<StaleBranch>> {
    let pr_heads: Vec<String> = deps
        .forge()
        .list_open_prs()
        .await?
        .into_iter()
        .map(|pr| pr.head_branch)
        .collect();
    let now = epoch_now() as i64;
    let mut stale = Vec::new();
    for branch in gitops::list_remote_branches(&deps.project.repo_path).await? {
        if branch.name == deps.project.default_branch || pr_heads.contains(&branch.name) {
            continue;
        }
        let age_days = (now.saturating_sub(branch.committer_unix)).max(0) as u64 / 86_400;
        // Errors (unknown refs, shallow history) count as "not merged".
        let merged = gitops::is_ancestor(
            &deps.project.repo_path,
            &format!("refs/remotes/origin/{}", branch.name),
            head_sha,
        )
        .unwrap_or(false);
        if merged || age_days > stale_days {
            stale.push(StaleBranch {
                name: branch.name,
                age_days,
                merged,
            });
        }
    }
    Ok(stale)
}

/// `meguri:working` labels with no active run behind them in this host's
/// store. Known limits, accepted because this is report-only: the list APIs
/// return open items only (a closed issue keeping the label is invisible),
/// and on multi-host setups another host's legitimate claim reads as an
/// orphan here (silence it via the ignore list).
async fn orphan_working(deps: &Deps) -> Result<Vec<OrphanWorking>> {
    let active: std::collections::HashSet<i64> = deps
        .store
        .list_runs(true)?
        .into_iter()
        .filter(|r| r.project_id == deps.project.id)
        .map(|r| r.issue_number)
        .collect();
    let mut orphans = Vec::new();
    for issue in deps
        .forge()
        .list_issues_with_label(forge::LABEL_WORKING)
        .await?
    {
        if !active.contains(&issue.number) {
            orphans.push(OrphanWorking {
                number: issue.number,
                is_pr: false,
                title: issue.title,
            });
        }
    }
    for pr in deps
        .forge()
        .list_prs_with_label(forge::LABEL_WORKING)
        .await?
    {
        if !active.contains(&pr.number) {
            orphans.push(OrphanWorking {
                number: pr.number,
                is_pr: true,
                title: pr.title,
            });
        }
    }
    Ok(orphans)
}

fn ignored(patterns: &[String], haystacks: &[&str]) -> bool {
    patterns
        .iter()
        .filter(|p| !p.is_empty())
        .any(|p| haystacks.iter().any(|h| h.contains(p.as_str())))
}

/// The full report body: marker first, then a human-readable snapshot. The
/// ignore list is applied here, at render time, so adding a pattern makes the
/// item vanish on the next sweep. Rendered even with zero findings — the
/// marker must advance or the same head would be swept again.
pub fn render_report(
    head: &str,
    scanned: u64,
    scanned_display: &str,
    findings: &[Finding],
    machine: &MachineFindings,
    ignore: &[String],
) -> String {
    let findings: Vec<&Finding> = findings
        .iter()
        .filter(|f| !ignored(ignore, &[&f.file, &f.note]))
        .collect();
    let stale: Vec<&StaleBranch> = machine
        .stale_branches
        .iter()
        .filter(|b| !ignored(ignore, &[&b.name]))
        .collect();
    let orphans: Vec<&OrphanWorking> = machine
        .orphan_working
        .iter()
        .filter(|o| !ignored(ignore, &[&format!("#{}", o.number)]))
        .collect();
    let phase_anomalies: Vec<&PhaseLabelAnomaly> = machine
        .phase_label_anomaly
        .iter()
        .filter(|a| !ignored(ignore, &[&format!("#{}", a.number)]))
        .collect();

    let short = head.get(..12).unwrap_or(head);
    let mut body = format!(
        "{marker}\n🔁 **meguri clean report** — a snapshot of the current \
         divergence at `{short}`, swept {scanned_display}. Items that are no \
         longer detected disappear on the next sweep.\n",
        marker = clean_marker(head, scanned),
    );

    body.push_str("\n## Agent findings\n");
    if findings.is_empty() {
        body.push_str("\n_No agent findings._\n");
    }
    for category in Category::ALL {
        let in_category: Vec<&&Finding> =
            findings.iter().filter(|f| f.category == category).collect();
        if in_category.is_empty() {
            continue;
        }
        body.push_str(&format!("\n### {}\n", category.heading()));
        for f in in_category {
            let location = match f.line {
                Some(line) => format!("`{}:{line}`", f.file),
                None => format!("`{}`", f.file),
            };
            body.push_str(&format!(
                "- {location} — {} _({})_\n",
                f.note,
                f.confidence.as_str()
            ));
        }
    }

    body.push_str("\n## Machine checks\n");
    if stale.is_empty() && orphans.is_empty() && phase_anomalies.is_empty() {
        body.push_str("\n_Nothing flagged._\n");
    }
    if !stale.is_empty() {
        body.push_str("\n### Stale branches\n");
        for b in &stale {
            let status = if b.merged {
                "merged into the default branch".to_string()
            } else {
                format!("last commit {} days ago", b.age_days)
            };
            body.push_str(&format!("- `{}` — {status}\n", b.name));
        }
    }
    if !orphans.is_empty() {
        body.push_str(&format!(
            "\n### Orphaned `{}` labels _(medium confidence)_\n",
            forge::LABEL_WORKING
        ));
        for o in &orphans {
            let kind = if o.is_pr { "PR" } else { "issue" };
            body.push_str(&format!("- {kind} #{} — {}\n", o.number, o.title));
        }
    }
    if !phase_anomalies.is_empty() {
        body.push_str("\n### Phase-label anomalies _(high confidence)_\n");
        for a in &phase_anomalies {
            let detail = if a.phases.len() >= 2 {
                format!(
                    "carries {} phase labels ({})",
                    a.phases.len(),
                    a.phases.join(", ")
                )
            } else {
                "has a ball label but no phase label".to_string()
            };
            body.push_str(&format!("- issue #{} — {detail}: {}\n", a.number, a.title));
        }
    }

    body.push_str(&format!(
        "\n---\nTo act on a finding, open a regular issue and label it \
         `{plan}` / `{ready}` — the existing loops take it from there. To \
         silence a false positive, add a substring pattern to `clean.ignore` \
         in the meguri config (editing this body doesn't stick; it is \
         rewritten every sweep). Pause the sweep with `{hold}` on this issue.\n",
        plan = forge::LABEL_PLAN,
        ready = forge::LABEL_READY,
        hold = forge::LABEL_HOLD,
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_roundtrip_including_none() {
        let m = clean_marker("abc123", 1_700_000_000);
        let parsed = parse_clean_marker(&format!("{m}\nreport body")).unwrap();
        assert_eq!(parsed.head, "abc123");
        assert_eq!(parsed.scanned, 1_700_000_000);

        let none = clean_marker(MARKER_HEAD_NONE, 42);
        let parsed = parse_clean_marker(&none).unwrap();
        assert_eq!(parsed.head, MARKER_HEAD_NONE);
        assert_eq!(parsed.scanned, 42);

        assert_eq!(parse_clean_marker("no marker here"), None);
        assert_eq!(parse_clean_marker("<!-- meguri:clean head=x -->"), None);
    }

    #[test]
    fn needs_scan_decision_table() {
        let day = 86_400;
        let m = |head: &str, scanned: u64| CleanMarker {
            head: head.into(),
            scanned,
        };
        // No marker: always scan.
        assert!(needs_scan(None, "abc", 1000, day));
        // Same head: never rescan, however much time passed.
        assert!(!needs_scan(Some(&m("abc", 0)), "abc", 10 * day, day));
        // Head moved but within the interval: wait.
        assert!(!needs_scan(
            Some(&m("old", 1000)),
            "abc",
            1000 + day - 1,
            day
        ));
        // Head moved and the interval elapsed: scan.
        assert!(needs_scan(Some(&m("old", 1000)), "abc", 1000 + day, day));
        // head=none (initializing marker) behaves like a moved head.
        assert!(!needs_scan(
            Some(&m(MARKER_HEAD_NONE, 1000)),
            "abc",
            1500,
            day
        ));
        assert!(needs_scan(
            Some(&m(MARKER_HEAD_NONE, 1000)),
            "abc",
            1000 + day,
            day
        ));
    }

    #[test]
    fn replace_marker_swaps_in_place_or_prepends() {
        let body = format!("{}\nfindings", clean_marker("old", 1));
        let updated = replace_marker(&body, &clean_marker("old", 99));
        assert_eq!(parse_clean_marker(&updated).unwrap().scanned, 99);
        assert!(updated.contains("findings"));
        assert_eq!(updated.matches("meguri:clean").count(), 1);

        let updated = replace_marker("plain body", &clean_marker("h", 2));
        assert!(updated.starts_with(&clean_marker("h", 2)));
        assert!(updated.contains("plain body"));
    }

    #[test]
    fn report_file_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".meguri")).unwrap();
        let path = dir.path().join(REPORT_FILE);

        let err = read_report(dir.path()).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");

        std::fs::write(&path, "not json").unwrap();
        let err = read_report(dir.path()).unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");

        std::fs::write(&path, r#"{"findings": []}"#).unwrap();
        assert!(read_report(dir.path()).unwrap().findings.is_empty());

        std::fs::write(
            &path,
            r#"{"findings": [{"category": "spec-drift", "file": "docs/specs/issue-1.md",
                "line": null, "note": "spec says X, code does Y", "confidence": "high"}]}"#,
        )
        .unwrap();
        let report = read_report(dir.path()).unwrap();
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].category, Category::SpecDrift);
        assert_eq!(report.findings[0].line, None);
        assert_eq!(report.findings[0].confidence, Confidence::High);
    }

    #[test]
    fn prompt_demands_report_not_changes() {
        let cp = CleanCheckpoint {
            head_sha: "deadbeef".into(),
            ..Default::default()
        };
        let prompt = execute_prompt(&cp, None);
        assert!(prompt.contains(REPORT_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("deadbeef"));
        assert!(prompt.contains("spec-drift"));
        assert!(!prompt.contains("# Output language"));

        let prompt = execute_prompt(&cp, Some("日本語"));
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    fn sample_findings() -> Vec<Finding> {
        vec![
            Finding {
                category: Category::SpecDrift,
                file: "docs/specs/issue-1.md".into(),
                line: Some(3),
                note: "spec still describes the old flow".into(),
                confidence: Confidence::High,
            },
            Finding {
                category: Category::Todo,
                file: "src/lib.rs".into(),
                line: None,
                note: "TODO from six months ago".into(),
                confidence: Confidence::Low,
            },
        ]
    }

    fn sample_machine() -> MachineFindings {
        MachineFindings {
            stale_branches: vec![StaleBranch {
                name: "meguri/old-thing".into(),
                age_days: 45,
                merged: false,
            }],
            orphan_working: vec![OrphanWorking {
                number: 12,
                is_pr: false,
                title: "left behind".into(),
            }],
            phase_label_anomaly: vec![
                PhaseLabelAnomaly {
                    number: 30,
                    title: "double phase".into(),
                    phases: vec![
                        forge::LABEL_SPECCING.to_string(),
                        forge::LABEL_IMPLEMENTING.to_string(),
                    ],
                    has_ball: false,
                },
                PhaseLabelAnomaly {
                    number: 31,
                    title: "ball no phase".into(),
                    phases: Vec::new(),
                    has_ball: true,
                },
            ],
        }
    }

    #[test]
    fn report_renders_categories_and_machine_sections() {
        let body = render_report(
            "0123456789abcdef",
            1_700_000_000,
            "2026-07-11T00:00:00Z",
            &sample_findings(),
            &sample_machine(),
            &[],
        );
        assert!(body.starts_with(&clean_marker("0123456789abcdef", 1_700_000_000)));
        assert!(body.contains("`0123456789ab`"), "{body}");
        assert!(body.contains("### Spec drift"));
        assert!(body.contains("`docs/specs/issue-1.md:3`"));
        assert!(body.contains("### Stranded TODO / FIXME"));
        assert!(body.contains("`src/lib.rs`"), "line-less finding: {body}");
        assert!(body.contains("### Stale branches"));
        assert!(body.contains("`meguri/old-thing` — last commit 45 days ago"));
        assert!(body.contains("issue #12 — left behind"));
        assert!(body.contains("### Phase-label anomalies"));
        assert!(body.contains("issue #30 — carries 2 phase labels"));
        assert!(body.contains("issue #31 — has a ball label but no phase label"));
        assert!(body.contains("clean.ignore"));
        // Empty categories render no heading.
        assert!(!body.contains("### Dead-code"));
    }

    #[test]
    fn ignore_list_drops_findings_branches_and_labels() {
        let ignore = vec![
            "docs/specs/issue-1.md".to_string(),
            "meguri/old-thing".to_string(),
            "#12".to_string(),
        ];
        let body = render_report(
            "head",
            1,
            "now",
            &sample_findings(),
            &sample_machine(),
            &ignore,
        );
        assert!(!body.contains("docs/specs/issue-1.md"));
        assert!(!body.contains("meguri/old-thing"));
        assert!(!body.contains("#12"));
        // The un-ignored finding survives.
        assert!(body.contains("src/lib.rs"));

        // A note match works too (substring, not path-only).
        let body = render_report(
            "head",
            1,
            "now",
            &sample_findings(),
            &MachineFindings::default(),
            &["six months ago".to_string()],
        );
        assert!(!body.contains("src/lib.rs"));
    }

    #[test]
    fn empty_report_still_carries_the_marker() {
        let body = render_report("h", 7, "now", &[], &MachineFindings::default(), &[]);
        assert!(body.contains(&clean_marker("h", 7)));
        assert!(body.contains("_No agent findings._"));
        assert!(body.contains("_Nothing flagged._"));
    }

    #[test]
    fn merged_branches_render_as_merged() {
        let machine = MachineFindings {
            stale_branches: vec![StaleBranch {
                name: "merged-one".into(),
                age_days: 0,
                merged: true,
            }],
            orphan_working: Vec::new(),
            phase_label_anomaly: Vec::new(),
        };
        let body = render_report("h", 1, "now", &[], &machine, &[]);
        assert!(body.contains("`merged-one` — merged into the default branch"));
    }
}
