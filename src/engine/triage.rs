//! The triage loop: a detector that periodically looks at every untriaged
//! open issue and rewrites a single per-project report issue
//! (`meguri:triage-report`) with a recommendation for each — how meguri
//! should handle it (`ready` / `plan` / `needs-human` / `hold` / `skip`),
//! how confident it is, and how big the work looks.
//!
//! It is the cleaner's twin one step further in (ADR 0006): the cleaner
//! automates *observation* (code/issue divergence), triage automates a
//! *decision* (what to do with an issue). So it stays read-only in v0 — its
//! write boundary is exactly that one report issue: no pushes, no branch
//! operations, and no labels or comments on the triaged issues themselves.
//! `advise` (v1, issue #87) carries the recommendation one step further onto
//! each recommended issue: a proposal label (`meguri:triage-ready` /
//! `-plan` / `-needs-human`) plus one evidence comment (confidence /
//! complexity / rationale / missing info) — still not a decision, since the
//! proposal labels are outside worker/planner discovery's vocabulary
//! (discovery keys on the exact real labels, never a `meguri:` prefix scan),
//! so a wrong proposal cannot start work on its own. Humans act on either
//! surface: adopt a recommendation by applying `meguri:ready`/`meguri:plan`
//! yourself (in `advise`, promoting the proposal label already there),
//! silence a bad one via `triage.ignore`, pause the sweep with `meguri:hold`
//! on the report issue.
//!
//! Opt-in: `[triage] mode` defaults to `off`; the loop sweeps on `report` and
//! `advise` (`auto`, v2 #88, still parses but stays idle). Re-scan is
//! rate-limited like the cleaner (default-branch head + interval) but adds
//! two more discovery signals, each independently checked and each still
//! interval-limited: an open issue numbered above the last scan's max
//! triggers a fresh sweep even while the head is still, so a new issue is
//! triaged without waiting for the next push (ADR 0006); and, in `advise`
//! mode, a previously proposed issue (still carrying only a `triage-*`
//! proposal label — a real workflow label always wins) whose content (title +
//! body) has drifted from its evidence comment's marker also triggers a
//! sweep, even with the head still and no new issue, so an edited proposal
//! is not stuck behind the next unrelated trigger. An unchanged proposed
//! issue is never resent to the agent.
//!
//! `advise`'s writes are idempotent and reversible: the evidence comment
//! carries a hidden `<!-- meguri:triage-advise hash=... -->` marker over the
//! recommended issue's content, so the same recommendation is never
//! re-proposed, and removing the proposal label (a human's rejection) is
//! respected until the content actually changes. `triage.max_actions_per_tick`
//! (default 3) caps how many issues a single sweep proposes to; the rest
//! carry over to the next sweep untouched.
//!
//! Lifetime mirrors the cleaner (issue #92): standalone, keyed by the report
//! issue, read-only detached worktree, self-reclaiming — the report issue
//! never closes, so triage releases its own pane and worktree at the end of
//! every sweep.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, STEP_EXECUTE, STEP_PREPARE_WORK, STEP_PREPARE_WORKTREE};
use super::{Deps, Target};
use crate::config::{TriageAction, TriageConfig, TriageMode};
use crate::forge::{self, Issue};
use crate::gitops;
use crate::store::{LANE_AUTHOR, RunRecord, RunStatus};
use crate::tasks::{self, TaskKey};
use crate::turn::{TurnOutcome, TurnStatus};

/// `runs.loop_kind` value for triage runs.
pub const KIND: &str = "triage";

/// Terminal triage step: render and write the report issue.
pub const STEP_SETTLE: &str = "settle";

/// Where the agent writes its recommendations (worktree-relative; `.meguri/`
/// is git-excluded, so it never dirties the read-only checkout).
pub const REPORT_FILE: &str = ".meguri/triage-report.json";

/// Title of the per-project report issue.
pub const REPORT_TITLE: &str = "🔀 meguri triage report";

/// Marker `head` value before any sweep completed.
pub const MARKER_HEAD_NONE: &str = "none";

/// Any label in meguri's own namespace. An open issue carrying one of these
/// is already engaged (a phase/ball/report/proposal label), so triage leaves
/// it alone — "no `meguri:` label = untriaged" (ADR 0005 / ADR 0006).
const WORKFLOW_LABEL_PREFIX: &str = "meguri:";

/// Hidden marker embedded in the report issue body: which head the report
/// covers, when the last sweep (or failed attempt) ran, the largest open
/// issue number seen at that time (the new-issue signal), and (`advise`
/// mode) whether that sweep left proposal backlog behind because
/// `max_actions_per_tick` cut it off. The issue body is the durable scan
/// state — nothing is kept locally ("Authority").
pub fn triage_marker(head: &str, scanned: u64, max_issue: i64, backlog: bool) -> String {
    let backlog = backlog as u8;
    format!(
        "<!-- meguri:triage head={head} scanned={scanned} max_issue={max_issue} backlog={backlog} -->"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageMarker {
    pub head: String,
    /// Unix epoch seconds of the last sweep attempt.
    pub scanned: u64,
    /// Largest open issue number at the last sweep (drives the new-issue
    /// signal: any open issue above this re-triggers a sweep).
    pub max_issue: i64,
    /// `advise` mode: the last sweep hit `max_actions_per_tick` with
    /// actionable recommendations still unwritten. Absent in a marker
    /// predating this field, which parses as `false` (no known backlog) —
    /// the same as a sweep that fully drained its recommendations.
    pub backlog: bool,
}

pub fn parse_triage_marker(body: &str) -> Option<TriageMarker> {
    let rest = body.split("<!-- meguri:triage ").nth(1)?;
    let fields = rest.split("-->").next()?;
    let mut head = None;
    let mut scanned = None;
    let mut max_issue = None;
    let mut backlog = false;
    for part in fields.split_whitespace() {
        if let Some(v) = part.strip_prefix("head=") {
            head = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("scanned=") {
            scanned = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("max_issue=") {
            max_issue = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("backlog=") {
            backlog = v == "1";
        }
    }
    Some(TriageMarker {
        head: head?,
        scanned: scanned?,
        max_issue: max_issue?,
        backlog,
    })
}

/// The discovery decision, pure so it unit-tests without a forge. Sweep when
/// nothing was ever recorded, or when *something changed* — the head moved,
/// an open issue numbered above the last scan appeared, the previous sweep
/// left `advise` proposal backlog behind (`marker.backlog`), or (`advise`
/// mode) a previously proposed issue's content drifted from its
/// evidence-comment marker — **and** the interval elapsed. The interval
/// rate-limits every signal, so a failed sweep that only advances `scanned`
/// still paces the retry. A truly unchanged state is never re-swept, however
/// old.
pub fn needs_triage_scan(
    marker: Option<&TriageMarker>,
    current_head: &str,
    max_open_issue: i64,
    now: u64,
    interval_secs: u64,
    advise_content_changed: bool,
) -> bool {
    match marker {
        None => true,
        Some(m) => {
            let changed = m.head != current_head
                || max_open_issue > m.max_issue
                || advise_content_changed
                || m.backlog;
            changed && now.saturating_sub(m.scanned) >= interval_secs
        }
    }
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The full discovery-due decision: [`needs_triage_scan`] plus the
/// per-project interval, shared by `discover` and `prepare_work` so both
/// re-verify the exact same condition. The `advise` content-drift signal
/// (`advise_backlog_changed`) is checked lazily — only when the cheap
/// signals (head, new-issue) don't already justify a sweep — since it costs
/// a full open-issues scan.
async fn scan_due(
    deps: &Deps,
    marker: Option<&TriageMarker>,
    head: &str,
    max_open: i64,
) -> Result<bool> {
    let interval = deps.config.triage_for(&deps.project).interval_hours * 3600;
    if needs_triage_scan(marker, head, max_open, epoch_now(), interval, false) {
        return Ok(true);
    }
    let advise_changed = advise_backlog_changed(deps).await?;
    Ok(needs_triage_scan(
        marker,
        head,
        max_open,
        epoch_now(),
        interval,
        advise_changed,
    ))
}

/// How meguri should handle an issue (v0 recommendation, never applied).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Recommendation {
    /// Scope clear, small/medium, no spec needed → a future `meguri:ready`.
    Ready,
    /// Ambiguous or large; spec-first warranted → a future `meguri:plan`.
    Plan,
    /// Underspecified or needs a human decision.
    NeedsHuman,
    /// Park it (question, blocked on discussion, ...).
    Hold,
    /// Out of scope (duplicate, wontfix-like, ...).
    Skip,
}

impl Recommendation {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Plan => "plan",
            Self::NeedsHuman => "needs-human",
            Self::Hold => "hold",
            Self::Skip => "skip",
        }
    }

    /// The inverse of [`as_str`], for reading a recommendation back out of a
    /// hidden marker. Unknown text yields `None` (a malformed or future marker).
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "ready" => Some(Self::Ready),
            "plan" => Some(Self::Plan),
            "needs-human" => Some(Self::NeedsHuman),
            "hold" => Some(Self::Hold),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }
}

/// Rough size of the work an issue implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    Small,
    Medium,
    Large,
}

impl Complexity {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

/// One agent recommendation, as written to [`REPORT_FILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageItem {
    pub issue: i64,
    pub recommendation: Recommendation,
    /// 0.0–1.0; the agent's own confidence, rated for a human reader.
    pub confidence: f64,
    pub estimated_complexity: Complexity,
    pub rationale: String,
    /// What to confirm before starting, if anything.
    #[serde(default)]
    pub missing_info: Option<String>,
}

/// What the agent writes to [`REPORT_FILE`].
#[derive(Debug, Deserialize)]
pub struct TriageReportFile {
    pub recommendations: Vec<TriageItem>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TriageCheckpoint {
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
    /// Marker max_issue found at claim time (kept on a skipped sweep so the
    /// new-issue signal is not recorded as consumed).
    #[serde(default)]
    pub prev_max_issue: i64,
    /// Marker backlog found at claim time — carried through a skipped sweep
    /// so a `GiveUp` that never reaches `apply_advise` doesn't silently drop
    /// a still-pending `advise` backlog from an earlier sweep.
    #[serde(default)]
    pub prev_backlog: bool,
    /// Verified agent recommendations, carried from execute to settle.
    #[serde(default)]
    pub recommendations: Vec<TriageItem>,
}

/// The triage loop: one report issue per project in, the same report issue
/// (rewritten) out.
pub struct TriageLoop;

#[async_trait]
impl super::Loop for TriageLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// One target at most: the project's report issue (or `0` when it does not
    /// exist yet — settle creates it; discovery itself stays read-only).
    /// Gated on `[triage] mode` being `report` or `advise`: the loop is
    /// opt-in, so it is a no-op until a human turns it on (`auto`, v2 #88,
    /// still parses but stays idle here too).
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.forge.is_none() {
            return Ok(Vec::new()); // forge-driven loop; inert in local mode
        }
        if !matches!(
            deps.config.triage_for(&deps.project).mode,
            TriageMode::Report | TriageMode::Advise | TriageMode::Auto
        ) {
            return Ok(Vec::new());
        }
        let issues = deps
            .forge()
            .list_issues_with_label(forge::LABEL_TRIAGE_REPORT)
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
            gitops::default_branch_head(&deps.repo_path(), &deps.project.default_branch).await?;
        let max_open = max_open_issue(deps).await?;
        let marker = report.as_ref().and_then(|i| parse_triage_marker(&i.body));
        if !scan_due(deps, marker.as_ref(), &head, max_open).await? {
            return Ok(Vec::new());
        }
        Ok(vec![Target {
            key: TaskKey::Issue(report.map(|i| i.number).unwrap_or(0)),
            title: REPORT_TITLE.to_string(),
            cadence_label: None,
        }])
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_triage(deps, run_id).await
    }
}

/// Largest open issue number on the forge, excluding the persistent report
/// issues meguri maintains (its own triage report and the cleaner's) — the
/// new-issue signal's input. Excluding them keeps the report issue's own
/// creation from re-triggering the next sweep, and a standing clean report from
/// reading as perpetual "new work". 0 when there are no other open issues.
async fn max_open_issue(deps: &Deps) -> Result<i64> {
    Ok(deps
        .forge()
        .list_open_issues()
        .await?
        .iter()
        .filter(|i| {
            !i.has_label(forge::LABEL_TRIAGE_REPORT) && !i.has_label(forge::LABEL_CLEAN_REPORT)
        })
        .map(|i| i.number)
        .max()
        .unwrap_or(0))
}

pub async fn run_triage(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
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
                    // mark_pane_reclaimed), like the cleaner — not a raw
                    // kill_pane, which would leave the pane row dangling.
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
                // Triage's drive never hands work to the planner; these are
                // unreachable here but recorded faithfully if they occur.
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
    let mut cp: TriageCheckpoint = serde_json::from_str(&run.checkpoint_json).unwrap_or_default();
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
                // instead of the poll, then reclaim the pane and worktree.
                settle_skip(deps, &run, &cp).await;
                super::reaper::release_pane(
                    deps,
                    run.issue_number,
                    LANE_AUTHOR,
                    "triage sweep gave up",
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
        // The report issue never closes, so the reaper would keep this pane
        // and detached worktree forever — triage reclaims them itself.
        super::reaper::release_pane(deps, run.issue_number, LANE_AUTHOR, "triage sweep finished")
            .await;
        remove_worktree_best_effort(deps, &run, &worktree).await;
        return Ok(WorkerOutcome::Succeeded {
            pr_url: format!("issue #{issue}"),
        });
    }

    bail!("unknown step {step:?}");
}

fn save_step(deps: &Deps, run: &RunRecord, step: &str, cp: &TriageCheckpoint) -> Result<String> {
    deps.store
        .update_run_step(&run.id, step, &serde_json::to_string(cp)?)?;
    Ok(step.to_string())
}

async fn remove_worktree_best_effort(deps: &Deps, run: &RunRecord, worktree: &Path) {
    if let Err(e) = gitops::remove_worktree(&deps.repo_path(), worktree).await {
        tracing::warn!(
            "cannot remove triage worktree {}: {e:#}",
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

/// prepare-work: re-verify the sweep is still due (hold, marker, interval,
/// new-issue signal) and pin the head + max_issue this run covers.
/// Deliberately no `meguri:working` claim — the write boundary is the report
/// issue alone (ADR 0003/0006); the run-index and the marker already dedup.
async fn prepare_work(deps: &Deps, run: &RunRecord, cp: &mut TriageCheckpoint) -> Result<Prepared> {
    let marker = if run.issue_number == 0 {
        // First sweep: if a report issue appeared since discovery (another
        // host, a manual one), defer to it — the next discovery targets its
        // real number and the unique run index applies.
        let issues = deps
            .forge()
            .list_issues_with_label(forge::LABEL_TRIAGE_REPORT)
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
        parse_triage_marker(&issue.body)
    };

    let head = gitops::default_branch_head(&deps.repo_path(), &deps.project.default_branch).await?;
    let max_open = max_open_issue(deps).await?;
    if !scan_due(deps, marker.as_ref(), &head, max_open).await? {
        return Ok(Prepared::Skip(format!(
            "head {head} needs no sweep (already scanned, within interval, or no new issue)"
        )));
    }

    cp.report_issue = run.issue_number;
    cp.head_sha = head;
    cp.prev_head = marker.as_ref().map(|m| m.head.clone()).unwrap_or_default();
    cp.prev_max_issue = marker.as_ref().map(|m| m.max_issue).unwrap_or(0);
    cp.prev_backlog = marker.as_ref().map(|m| m.backlog).unwrap_or(false);
    deps.store.emit(
        Some(&run.id),
        "triage.claimed",
        json!({ "issue": cp.report_issue, "head": cp.head_sha }),
    )?;
    Ok(Prepared::Ready)
}

/// prepare-worktree: read-only detached checkout of the default branch head
/// (same mechanism as the cleaner / the reviewer's PR-head checkout).
async fn prepare_worktree(deps: &Deps, run: &RunRecord, cp: &TriageCheckpoint) -> Result<()> {
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let dir = format!("triage-{}", run.id);
    let wt = gitops::worktree_path(&root, &deps.project.id, &dir);
    gitops::create_review_worktree(
        &deps.repo_path(),
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

/// A "real" meguri workflow label (any `meguri:` label except triage's own
/// `advise` proposal labels): an issue carrying one is already engaged by
/// another loop, or held, so triage leaves it alone regardless of content.
/// Excluding the proposal labels from this check is what lets a
/// proposed-but-not-yet-decided issue come back for re-triage once its
/// content changes (ADR 0006 point 2 / issue #87).
fn is_engaged_label(label: &str) -> bool {
    label.starts_with(WORKFLOW_LABEL_PREFIX) && !forge::TRIAGE_PROPOSAL_LABELS.contains(&label)
}

/// The open issues this sweep considers: not engaged by another loop or held,
/// no unresolved blocker, and not already advised on this exact content — an
/// issue whose latest advise evidence marker still matches its title/body is
/// excluded (recovering v0's ADR-0006 point 2 TODO: re-triage follows
/// content, not just label absence). The marker check deliberately ignores
/// whether a proposal label is still present: a human rejecting a proposal
/// removes the label but not the evidence comment, and the promise "a
/// rejected proposal stays rejected until the content changes" has to hold
/// here too, not only in `propose_one` — otherwise the rejected issue would
/// re-enter the candidate list and re-appear in the central report on the
/// next unrelated sweep. Sorted by number for a stable prompt and report.
async fn gather_candidates(deps: &Deps) -> Result<Vec<Issue>> {
    let mut candidates = Vec::new();
    for issue in deps.forge().list_open_issues().await? {
        if issue.labels.iter().any(|l| is_engaged_label(l)) {
            continue;
        }
        if has_unresolved_blockers(deps, issue.number).await {
            continue;
        }
        if !content_changed_since_advise(deps, &issue).await? {
            continue;
        }
        candidates.push(issue);
    }
    candidates.sort_by_key(|i| i.number);
    Ok(candidates)
}

/// SHA-256 fingerprint of an issue's title + body — what counts as "the same
/// content" for `advise`'s idempotency marker (reuses the reconcile loop's
/// whitespace-normalized digest, issue #142).
fn content_hash(issue: &Issue) -> String {
    tasks::body_digest(&format!("{}\n{}", issue.title, issue.body))
}

/// Whether triage still has an action to take on `issue`, judged from its
/// latest evidence-comment marker. Content that moved since the marker is
/// always eligible (re-triage). At unchanged content the answer depends on the
/// mode and the marker's applied level (ADR 0017 decision 3):
///
/// - `report`/`advise`: an unchanged marker means "already actioned" — not
///   eligible (v1 idempotency, preserved verbatim).
/// - `auto`: a `real` marker means already promoted (or promoted then reverted
///   = a human rejection) — not eligible. A `proposal` marker means the last
///   action was only a v1 proposal: eligible **iff a proposal label is still
///   present AND its recommendation is promotable under the current `apply`**
///   (a pending proposal `auto` could actually escalate). If the label was
///   removed, the human rejected it; if its kind is outside `apply` (e.g. a v1
///   `plan` proposal under the default `apply = ["ready"]`), escalation would
///   always no-op — treating either as pending would re-scan the issue every
///   interval forever.
///
/// `no_marker` is the answer when the issue has no marker at all — `true` for
/// candidate gathering (a never-triaged issue is a candidate), `false` for the
/// drift re-scan signal (a never-proposed issue is not "drift", or every
/// untriaged issue would re-trigger the sweep and defeat the rate limit).
async fn triage_action_pending(deps: &Deps, issue: &Issue, no_marker: bool) -> Result<bool> {
    let Some(marker) = latest_advise_marker(deps, issue.number).await? else {
        return Ok(no_marker);
    };
    if marker.hash != content_hash(issue) {
        return Ok(true); // content moved: re-triage
    }
    let cfg = deps.config.triage_for(&deps.project);
    if cfg.mode != TriageMode::Auto {
        return Ok(false); // report/advise: unchanged marker = already actioned
    }
    match marker.applied {
        // Already promoted (or reverted = rejection), or auto already evaluated
        // and declined this content: settled, not pending.
        AppliedLevel::Real | AppliedLevel::Declined => Ok(false),
        AppliedLevel::Proposal => {
            // Pending → escalate; rejected (label gone) or kind outside `apply`
            // (escalation would no-op) → not pending, so this doesn't re-scan
            // forever. Below-threshold / ignored proposals that reach the sweep
            // get an explicit `Declined` marker (above), which lands here on the
            // next pass — that is what stops the loop for those cases.
            let promotable = marker
                .recommendation
                .is_some_and(|rec| promote_label(rec, &cfg.apply).is_some());
            Ok(has_proposal_label(issue) && promotable)
        }
    }
}

/// Whether `issue` carries any triage `advise` proposal label.
fn has_proposal_label(issue: &Issue) -> bool {
    forge::TRIAGE_PROPOSAL_LABELS
        .iter()
        .any(|l| issue.has_label(l))
}

/// Whether an issue is eligible for a fresh triage recommendation. No marker at
/// all (never proposed, a proposal label applied by hand, or a marker that
/// predates this feature) counts as eligible, so the issue is offered.
async fn content_changed_since_advise(deps: &Deps, issue: &Issue) -> Result<bool> {
    triage_action_pending(deps, issue, true).await
}

/// The discovery-time drift signal: whether triage has a pending action on
/// `issue` per its marker. Unlike `content_changed_since_advise` (used by
/// `gather_candidates`, where "never proposed" should also count as eligible),
/// a genuinely never-proposed issue returns `false` here — it has no marker to
/// drift from, and treating every ordinary untriaged issue as drifted would
/// defeat the point of rate-limiting this signal.
async fn marker_drifted(deps: &Deps, issue: &Issue) -> Result<bool> {
    triage_action_pending(deps, issue, false).await
}

/// The discovery-time counterpart of `content_changed_since_advise`: whether
/// *any* open, non-engaged issue has a pending triage action per its
/// evidence-comment marker. Without this, editing a proposed issue's
/// title/body alone (no new issue filed, no default-branch push) never sets
/// `needs_triage_scan`'s `changed` flag, so the sweep that would notice the
/// drift — and `gather_candidates`'s own per-issue check — is never even
/// scheduled. In `auto` mode it also fires on a still-pending proposal at
/// unchanged content (a proposal label present with a `proposal`-level
/// marker), so switching `advise`→`auto` actually reaches the promotion path
/// (`marker_drifted` encodes that; ADR 0017). `off`/`report` never act
/// per-issue, so this short-circuits to `false` there.
async fn advise_backlog_changed(deps: &Deps) -> Result<bool> {
    if !matches!(
        deps.config.triage_for(&deps.project).mode,
        TriageMode::Advise | TriageMode::Auto
    ) {
        return Ok(false);
    }
    for issue in deps.forge().list_open_issues().await? {
        if issue.labels.iter().any(|l| is_engaged_label(l)) {
            continue; // a real workflow label or hold: not triage's business
        }
        if marker_drifted(deps, &issue).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Dependency gate (looper ADR-0004): a GitHub-native `blocked_by` that isn't
/// closed-as-completed keeps the issue out of triage. Unreadable blockers count
/// as unresolved, never the reverse.
async fn has_unresolved_blockers(deps: &Deps, issue: i64) -> bool {
    match deps.forge().blocked_by(issue).await {
        Ok(blockers) => blockers.iter().any(|b| !b.resolved()),
        Err(_) => true,
    }
}

fn execute_prompt(candidates: &[Issue], sha: &str, language: Option<&str>) -> String {
    let mut issues_block = String::new();
    for c in candidates {
        issues_block.push_str(&format!(
            "## Issue #{}: {}\n\n{}\n\n",
            c.number, c.title, c.body
        ));
    }
    format!(
        "You are triaging this repository's untriaged open issues. The worktree \
         is a read-only checkout of the default branch head (commit `{sha}`).\n\n\
         # Issues to triage\n\n{issues_block}\
         # Instructions\n\
         - For each issue above, investigate the repository (read-only) and \
           decide how meguri should handle it. Pick one recommendation:\n\
           - `ready`: scope is clear and small/medium, no spec needed.\n\
           - `plan`: ambiguous or large; a spec-first pass is warranted.\n\
           - `needs-human`: underspecified or needs a human decision.\n\
           - `hold`: park it (a question, blocked on discussion, ...).\n\
           - `skip`: out of scope (duplicate, wontfix-like, ...).\n\
         - Do NOT modify, commit, or push anything, and do NOT comment on or \
           label any issue; the report file below is your only deliverable.\n\
         - Write your recommendations to `{report}` as JSON:\n\
           `{{\"recommendations\": [{{\"issue\": 81, \"recommendation\": \
           \"ready\" | \"plan\" | \"needs-human\" | \"hold\" | \"skip\", \
           \"confidence\": 0.0, \"estimated_complexity\": \"small\" | \
           \"medium\" | \"large\", \"rationale\": \"<1-2 sentences on why>\", \
           \"missing_info\": \"<what to confirm first, or omit>\"}}]}}`\n\
           - `confidence` is 0.0-1.0; rate it honestly, a human reads this.\n\
           - `missing_info` is optional (omit or leave empty when none).\n\
           - Cover every issue listed above; skip none.\n\
         - An empty list is valid when there is nothing to triage: \
           `{{\"recommendations\": []}}`.\n\
         - A completed triage is a success regardless of the recommendations; \
           report \"failure\"/\"needs_human\" only when you cannot triage at all.\
         {lang_section}",
        report = REPORT_FILE,
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

/// The triage deliverable, verified after each turn: a parseable report file
/// and an untouched checkout. The Err text feeds a corrective prompt.
fn read_report(worktree: &Path) -> std::result::Result<TriageReportFile, String> {
    let raw = std::fs::read_to_string(worktree.join(REPORT_FILE)).map_err(|_| {
        format!("- report file `{REPORT_FILE}` does not exist (write it as instructed)")
    })?;
    serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- report file `{REPORT_FILE}` is not valid JSON ({e}); expected \
             {{\"recommendations\": [{{\"issue\", \"recommendation\", \
             \"confidence\", \"estimated_complexity\", \"rationale\", \
             \"missing_info\"}}]}}"
        )
    })
}

/// Verify the report covers exactly the issues it was asked to triage: one
/// recommendation per candidate, no duplicates, none for an issue outside the
/// list. Returns a corrective message otherwise — without this a report that
/// silently drops (or invents) issues would still be accepted, and settle would
/// advance the marker over the dropped issues, leaving them un-triaged until the
/// head or issue set next moves. Only called with a non-empty candidate set (an
/// empty one is short-circuited before the agent runs).
fn coverage_problem(candidates: &[Issue], recs: &[TriageItem]) -> Option<String> {
    use std::collections::BTreeSet;
    let expected: BTreeSet<i64> = candidates.iter().map(|i| i.number).collect();
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    let mut duplicate: BTreeSet<i64> = BTreeSet::new();
    let mut out_of_scope: BTreeSet<i64> = BTreeSet::new();
    for r in recs {
        if !expected.contains(&r.issue) {
            out_of_scope.insert(r.issue);
        } else if !seen.insert(r.issue) {
            duplicate.insert(r.issue);
        }
    }
    let missing: Vec<i64> = expected.difference(&seen).copied().collect();
    if missing.is_empty() && duplicate.is_empty() && out_of_scope.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("no recommendation for issue(s) {missing:?}"));
    }
    if !duplicate.is_empty() {
        let d: Vec<i64> = duplicate.into_iter().collect();
        parts.push(format!("more than one recommendation for issue(s) {d:?}"));
    }
    if !out_of_scope.is_empty() {
        let o: Vec<i64> = out_of_scope.into_iter().collect();
        parts.push(format!(
            "a recommendation for issue(s) {o:?} that were not in the triage list"
        ));
    }
    Some(format!(
        "- the report must cover exactly the issues listed for triage, one \
         recommendation each: {}. Fix `{REPORT_FILE}` to include every listed \
         issue exactly once and none other.",
        parts.join("; ")
    ))
}

enum ExecuteFlow {
    Verified,
    Stopped,
    Interrupted(String),
    /// The agent could not produce a verifiable report (even after the
    /// corrective turn) — give up quietly; the next interval retries.
    GiveUp(String),
}

/// execute: gather the untriaged issues, run one triage turn plus at most one
/// corrective turn. With nothing to triage the agent is skipped entirely (an
/// empty report still advances the marker). Like the cleaner, a persistently
/// failing agent is NOT escalated — nothing is lost on a read-only sweep, so
/// the run gives up quietly.
async fn execute(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut TriageCheckpoint,
    worktree: &Path,
) -> Result<ExecuteFlow> {
    let candidates = gather_candidates(deps).await?;
    if candidates.is_empty() {
        cp.recommendations = Vec::new();
        deps.store.emit(
            Some(&run.id),
            "triage.verified",
            json!({ "recommendations": 0, "candidates": 0, "head": cp.head_sha }),
        )?;
        return Ok(ExecuteFlow::Verified);
    }

    let mut prompt = execute_prompt(
        &candidates,
        &cp.head_sha,
        deps.config.language_for(&deps.project),
    );
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = flow::run_turn(deps, run, worktree, "triage", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(ExecuteFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(ExecuteFlow::Interrupted("pane died during triage".into()));
            }
            // Normalized inside run_turn_in (issue #245); kept for exhaustiveness.
            TurnOutcome::AgentQuiet { .. } => {
                return Ok(ExecuteFlow::Interrupted(
                    "agent went quiet during triage".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            // needs_plan is a worker signal and decompose a planner one; on a
            // read-only sweep they make no sense — give up like failure.
            TurnStatus::Failure
            | TurnStatus::NeedsHuman
            | TurnStatus::NeedsPlan
            | TurnStatus::Decompose => {
                return Ok(ExecuteFlow::GiveUp(format!(
                    "agent could not complete the triage: {}",
                    result.summary
                )));
            }
        }

        // Trust but verify: the checkout must be pristine and still at the
        // claimed head, the report file must parse, and it must cover exactly
        // the issues we asked about.
        let clean = gitops::status_clean(worktree).await?;
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        let problem = if !clean || head_now != cp.head_sha {
            Some(format!(
                "- the triage checkout must stay untouched: working tree clean: \
                 {clean} (must be true), HEAD: {head_now} (must be {expected}) — \
                 discard all changes (`git checkout -- . && git clean -fd && \
                 git reset --hard {expected}`), the report file under .meguri/ \
                 is exempt",
                expected = cp.head_sha,
            ))
        } else {
            match read_report(worktree) {
                Err(e) => Some(e),
                Ok(report) => coverage_problem(&candidates, &report.recommendations),
            }
        };
        let Some(problem) = problem else {
            let report = read_report(worktree).expect("verified above");
            cp.recommendations = report.recommendations;
            deps.store.emit(
                Some(&run.id),
                "triage.verified",
                json!({
                    "recommendations": cp.recommendations.len(),
                    "candidates": candidates.len(),
                    "head": cp.head_sha,
                }),
            )?;
            return Ok(ExecuteFlow::Verified);
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Ok(ExecuteFlow::GiveUp(format!(
                "triage doesn't verify after a corrective turn:\n{problem}"
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

/// Quiet-skip bookkeeping: advance only the marker's `scanned` (the head and
/// max_issue are NOT recorded as swept), creating an "initializing" report
/// issue when none exists yet. Without this, a permanently failing agent would
/// burn a turn every poll instead of every interval. Best-effort: a failed
/// write just means the next poll retries sooner. This create/update is still
/// the loop's only forge write.
async fn settle_skip(deps: &Deps, run: &RunRecord, cp: &TriageCheckpoint) {
    let prev_head = if cp.prev_head.is_empty() {
        MARKER_HEAD_NONE
    } else {
        &cp.prev_head
    };
    let marker = triage_marker(prev_head, epoch_now(), cp.prev_max_issue, cp.prev_backlog);
    if cp.report_issue == 0 {
        let body = format!(
            "{marker}\n🔀 **meguri triage report** — initializing: the first \
             sweep did not complete. meguri retries after the configured \
             interval (`triage.interval_hours`)."
        );
        match deps
            .forge()
            .create_issue(REPORT_TITLE, &body, &[forge::LABEL_TRIAGE_REPORT])
            .await
        {
            Ok(number) => {
                let _ = deps.store.emit(
                    Some(&run.id),
                    "triage.report_created",
                    json!({ "issue": number, "initializing": true }),
                );
                deps.notify_created_issue(number, REPORT_TITLE, &[forge::LABEL_TRIAGE_REPORT])
                    .await;
            }
            Err(e) => tracing::warn!("cannot create initializing triage report issue: {e:#}"),
        }
        return;
    }
    let body = match deps.forge().get_issue(cp.report_issue).await {
        Ok(issue) => replace_marker(&issue.body, &marker),
        Err(e) => {
            tracing::warn!(
                "cannot re-read triage report issue #{}: {e:#}",
                cp.report_issue
            );
            return;
        }
    };
    if let Err(e) = deps.forge().update_issue_body(cp.report_issue, &body).await {
        tracing::warn!(
            "cannot update triage report issue #{}: {e:#}",
            cp.report_issue
        );
    }
}

/// Swap the marker line inside an existing body (or prepend one).
fn replace_marker(body: &str, marker: &str) -> String {
    let Some(start) = body.find("<!-- meguri:triage ") else {
        return format!("{marker}\n{body}");
    };
    let Some(len) = body[start..].find("-->").map(|i| i + 3) else {
        return format!("{marker}\n{body}");
    };
    format!("{}{}{}", &body[..start], marker, &body[start + len..])
}

/// settle: `advise` proposes (label + evidence comment) and `auto` promotes
/// (real phase label + reason comment) on the recommended issues themselves;
/// then, always, render the snapshot body and write the report issue.
/// `max_issue` is recorded fresh here (so the just-created report issue does
/// not re-trigger the next sweep). Returns the report issue number.
async fn settle(deps: &Deps, run: &RunRecord, cp: &TriageCheckpoint) -> Result<i64> {
    let triage_cfg = deps.config.triage_for(&deps.project);
    let ignore = triage_cfg.ignore.clone();
    let mode = triage_cfg.mode;
    let (promoted, backlog) = match mode {
        TriageMode::Advise => (
            Vec::new(),
            apply_advise(deps, run, &cp.recommendations).await,
        ),
        TriageMode::Auto => apply_auto(deps, run, &cp.recommendations).await,
        TriageMode::Off | TriageMode::Report => (Vec::new(), false),
    };
    let max_open = max_open_issue(deps).await.unwrap_or(cp.prev_max_issue);
    let body = render_report(
        &cp.head_sha,
        epoch_now(),
        &crate::store::now(),
        max_open,
        &cp.recommendations,
        &ignore,
        mode,
        backlog,
        &promoted,
    );

    let issue = if cp.report_issue == 0 {
        let number = deps
            .forge()
            .create_issue(REPORT_TITLE, &body, &[forge::LABEL_TRIAGE_REPORT])
            .await?;
        deps.store.emit(
            Some(&run.id),
            "triage.report_created",
            json!({ "issue": number }),
        )?;
        deps.notify_created_issue(number, REPORT_TITLE, &[forge::LABEL_TRIAGE_REPORT])
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
        "triage.reported",
        json!({
            "issue": issue,
            "head": cp.head_sha,
            "recommendations": cp.recommendations.len(),
            "max_issue": max_open,
            "backlog": backlog,
        }),
    )?;
    Ok(issue)
}

/// `advise` write path: propose (label + evidence comment) on up to
/// `triage.max_actions_per_tick` recommended issues. Best-effort — one
/// issue's failure (closed mid-run, forge hiccup, ...) is logged and skipped
/// rather than failing the whole sweep; the report and the scan marker still
/// get written regardless. Returns whether actionable backlog remains — a
/// recommendation that could have proposed something but didn't (budget cut
/// it off, or the write itself failed) — which the caller records on the
/// marker so the next `needs_triage_scan` fires on it even if neither the
/// head nor the open-issue set otherwise moved.
async fn apply_advise(deps: &Deps, run: &RunRecord, recommendations: &[TriageItem]) -> bool {
    let cfg = deps.config.triage_for(&deps.project);
    let ignore = cfg.ignore.clone();
    let budget = cfg.max_actions_per_tick;
    let mut used = 0u64;
    let mut backlog = false;
    for item in recommendations {
        if used >= budget {
            // Budget cut it off before we could even check whether it's a
            // genuine no-op (already engaged, ignored, unchanged hash) — an
            // occasionally-overeager but safe approximation: the next sweep
            // re-checks properly, this only decides whether one is due.
            if proposal_label(item.recommendation).is_some() {
                backlog = true;
            }
            continue;
        }
        match propose_one(deps, item, &ignore).await {
            Ok(true) => {
                used += 1;
                let _ = deps.store.emit(
                    Some(&run.id),
                    "triage.advised",
                    json!({ "issue": item.issue, "recommendation": item.recommendation.as_str() }),
                );
            }
            Ok(false) => {}
            Err(e) => {
                // The attempt itself failed — the hash didn't move, so this
                // is backlog too, not a resolved no-op.
                backlog = true;
                tracing::warn!("triage advise on issue #{}: {e:#}", item.issue);
            }
        }
    }
    backlog
}

/// One issue's proposal attempt: label + evidence comment, gated by `ignore`,
/// a human override check re-read fresh (not the possibly-stale candidate
/// snapshot from gather time), and the hidden-marker idempotency check.
/// Returns whether an action was actually taken (this is what counts against
/// `max_actions_per_tick`).
async fn propose_one(deps: &Deps, item: &TriageItem, ignore: &[String]) -> Result<bool> {
    let Some(label) = proposal_label(item.recommendation) else {
        return Ok(false); // hold/skip: report-only, no per-issue action
    };
    if ignored(
        ignore,
        &[
            &format!("#{}", item.issue),
            item.recommendation.as_str(),
            &item.rationale,
            item.missing_info.as_deref().unwrap_or(""),
        ],
    ) {
        return Ok(false);
    }
    let issue = deps.forge().get_issue(item.issue).await?;
    if issue.labels.iter().any(|l| is_engaged_label(l)) {
        return Ok(false); // a human already acted, or the issue is held
    }
    let hash = content_hash(&issue);
    if let Some(prev) = latest_advise_marker(deps, item.issue).await?
        && prev.hash == hash
    {
        // Unchanged since the last proposal — already proposed (label may
        // still be there) or rejected (a human removed it). Either way, the
        // content hasn't moved, so there is nothing new to say.
        return Ok(false);
    }
    for stale in forge::TRIAGE_PROPOSAL_LABELS {
        if stale != label && issue.has_label(stale) {
            deps.forge().remove_label(item.issue, stale).await?;
        }
    }
    deps.forge().add_label(item.issue, label).await?;
    deps.forge()
        .comment(item.issue, &render_advise_comment(item, &hash))
        .await?;
    Ok(true)
}

/// `auto` write path: promote (real phase label + reason comment) up to
/// `triage.max_actions_per_tick` recommendations that clear `confidence_threshold`
/// and are listed in `apply`. Best-effort like `apply_advise` — one issue's
/// failure is logged and skipped. Returns `(promoted issue numbers, backlog)`,
/// where `backlog` means a promotable recommendation was left unwritten (budget
/// cut it off, or the write failed), so the caller records it on the marker and
/// the next `needs_triage_scan` fires even if nothing else moved.
async fn apply_auto(
    deps: &Deps,
    run: &RunRecord,
    recommendations: &[TriageItem],
) -> (Vec<i64>, bool) {
    let cfg = deps.config.triage_for(&deps.project);
    let ignore = cfg.ignore.clone();
    let budget = cfg.max_actions_per_tick;
    let mut used = 0u64;
    let mut promoted = Vec::new();
    let mut backlog = false;
    for item in recommendations {
        // "Could this recommendation ever promote?" — in `apply`, and over the
        // confidence bar. Sub-threshold / non-`apply` items are never backlog.
        let promotable = promote_label(item.recommendation, &cfg.apply).is_some()
            && item.confidence >= cfg.confidence_threshold;
        if used >= budget {
            if promotable {
                backlog = true;
            }
            continue;
        }
        match promote_one(deps, item, cfg, &ignore).await {
            Ok(true) => {
                used += 1;
                promoted.push(item.issue);
                let label = promote_label(item.recommendation, &cfg.apply).unwrap_or_default();
                let _ = deps.store.emit(
                    Some(&run.id),
                    "triage.promoted",
                    json!({
                        "issue": item.issue,
                        "recommendation": item.recommendation.as_str(),
                        "label": label,
                        "confidence": item.confidence,
                    }),
                );
            }
            Ok(false) => {}
            Err(e) => {
                // The attempt failed — the hash didn't move, so a promotable
                // item is still backlog, not a resolved no-op.
                if promotable {
                    backlog = true;
                }
                tracing::warn!("triage auto-promote on issue #{}: {e:#}", item.issue);
            }
        }
    }
    (promoted, backlog)
}

/// One issue's promotion attempt: apply the real phase label + a reason
/// comment, gated by `apply`/`confidence_threshold`, `ignore`, a fresh
/// human-override re-read, and the marker idempotency/rejection check. Returns
/// whether a label was actually applied (what counts against the budget).
async fn promote_one(
    deps: &Deps,
    item: &TriageItem,
    cfg: &TriageConfig,
    ignore: &[String],
) -> Result<bool> {
    let issue = deps.forge().get_issue(item.issue).await?;
    if issue.labels.iter().any(|l| is_engaged_label(l)) {
        return Ok(false); // a real label already, or held — never override a human
    }
    let hash = content_hash(&issue);
    // Read the latest marker first: it decides both the settled-state early
    // returns and whether this issue is a *standing re-scan signal* (any marker
    // = drift or pending), which is what a decline has to settle.
    let prev = latest_advise_marker(deps, item.issue).await?;
    if let Some(m) = &prev
        && m.hash == hash
    {
        // Same content as the last triage action. Suppress when it was already
        // a real promotion (idempotent, or a human reverted it = rejection),
        // when auto already declined this content, or when a prior proposal
        // here was rejected (proposal marker, no proposal label left). Escalate
        // only a still-pending proposal.
        match m.applied {
            AppliedLevel::Real | AppliedLevel::Declined => return Ok(false),
            AppliedLevel::Proposal if !has_proposal_label(&issue) => return Ok(false),
            AppliedLevel::Proposal => {}
        }
    }
    // Would auto promote this? It must be a promotable kind listed in `apply`,
    // clear the confidence bar, and not be silenced by `triage.ignore`. Every
    // other outcome (kind not in `apply`, needs-human/hold/skip, below
    // threshold, ignored) is a no-op — and when the issue already carries a
    // triage marker it is a standing re-scan signal (a pending proposal, or
    // drift from a stale-hash marker whose content just changed), so the no-op
    // has to be recorded at the current content or the sweep re-triages it every
    // interval. A fresh issue with no marker is not a re-scan signal, so its
    // no-op records nothing.
    let promotable = promote_label(item.recommendation, &cfg.apply).filter(|_| {
        item.confidence >= cfg.confidence_threshold
            && !ignored(
                ignore,
                &[
                    &format!("#{}", item.issue),
                    item.recommendation.as_str(),
                    &item.rationale,
                    item.missing_info.as_deref().unwrap_or(""),
                ],
            )
    });
    let Some(label) = promotable else {
        if prev.is_some() {
            record_decline(deps, item, &hash).await;
        }
        return Ok(false);
    };
    // Apply the real label, then the reason comment, then (only on success)
    // supersede the proposal labels. Ordering guards auto's "reason comment
    // mandatory / removable to revert" invariant against a partial write: a
    // bare real label with no reason comment would still engage the issue for
    // worker/planner while leaving nothing to explain or audit it. So if the
    // comment fails we roll the label back and let the next sweep retry from a
    // clean state — and the proposal labels are removed last, so a comment
    // failure leaves them untouched (nothing to restore) rather than making the
    // issue look rejected.
    deps.forge().add_label(item.issue, label).await?;
    if let Err(e) = deps
        .forge()
        .comment(item.issue, &render_promote_comment(item, label, &hash))
        .await
    {
        if let Err(re) = deps.forge().remove_label(item.issue, label).await {
            tracing::warn!(
                "triage auto-promote #{}: reason comment failed and rolling back \
                 the {label} label also failed ({re:#}) — the issue may carry \
                 {label} without a reason comment",
                item.issue
            );
        }
        return Err(e).with_context(|| {
            format!(
                "posting the auto-promote reason comment for #{}",
                item.issue
            )
        });
    }
    // The label + comment landed. Superseding the proposal labels is cosmetic
    // now (the real label already engages the issue), so a failure here is
    // logged, not propagated — it must not undo a completed promotion.
    for stale in forge::TRIAGE_PROPOSAL_LABELS {
        if issue.has_label(stale)
            && let Err(e) = deps.forge().remove_label(item.issue, stale).await
        {
            tracing::warn!(
                "triage auto-promote #{}: removing superseded proposal label {stale}: {e:#}",
                item.issue
            );
        }
    }
    Ok(true)
}

/// The `advise` proposal label for a recommendation, or `None` for `hold`/
/// `skip` — those stay report-only, there is nothing actionable to propose.
fn proposal_label(rec: Recommendation) -> Option<&'static str> {
    match rec {
        Recommendation::Ready => Some(forge::LABEL_TRIAGE_READY),
        Recommendation::Plan => Some(forge::LABEL_TRIAGE_PLAN),
        Recommendation::NeedsHuman => Some(forge::LABEL_TRIAGE_NEEDS_HUMAN),
        Recommendation::Hold | Recommendation::Skip => None,
    }
}

/// The real workflow label a human promotes the proposal to (and the one
/// `auto` mode applies directly).
fn real_label(rec: Recommendation) -> Option<&'static str> {
    match rec {
        Recommendation::Ready => Some(forge::LABEL_READY),
        Recommendation::Plan => Some(forge::LABEL_PLAN),
        Recommendation::NeedsHuman => Some(forge::LABEL_NEEDS_HUMAN),
        Recommendation::Hold | Recommendation::Skip => None,
    }
}

/// The real phase label `auto` mode would promote `rec` to, or `None` when it
/// is not promotable in this config: only `ready`/`plan` (the two-axis phase
/// labels, ADR 0005) and only when the recommendation is listed in `apply`.
/// `needs-human`/`hold`/`skip` are never promoted (ADR 0017 decision 8).
fn promote_label(rec: Recommendation, apply: &[TriageAction]) -> Option<&'static str> {
    let action = match rec {
        Recommendation::Ready => TriageAction::Ready,
        Recommendation::Plan => TriageAction::Plan,
        Recommendation::NeedsHuman | Recommendation::Hold | Recommendation::Skip => return None,
    };
    apply.contains(&action).then(|| real_label(rec)).flatten()
}

/// How far triage acted on an issue at a given content hash. `advise` writes a
/// `Proposal` marker (a proposal label + evidence comment); `auto` writes a
/// `Real` marker (a real phase label was applied), or a `Declined` marker when
/// it evaluated a pending proposal and chose not to promote it (below
/// `confidence_threshold`, or silenced by `ignore`). Distinguishing them lets
/// `auto` escalate a still-pending proposal, respect an already-made promotion
/// / rejection (ADR 0017 decision 3), and — via `Declined` — stop re-triaging a
/// proposal it has already decided not to promote, until the content changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppliedLevel {
    Proposal,
    Real,
    Declined,
}

impl AppliedLevel {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Proposal => "proposal",
            Self::Real => "real",
            Self::Declined => "declined",
        }
    }
}

/// Hidden marker embedded in a triage evidence comment: the content hash the
/// action was taken against, plus the applied level. Idempotency (don't repeat
/// the same action), rejection-respect (don't re-act after a human removes the
/// label, as long as the content hasn't moved), and the `advise`→`auto`
/// escalation all read off these fields.
fn advise_marker(hash: &str, recommendation: Recommendation, applied: AppliedLevel) -> String {
    format!(
        "<!-- meguri:triage-advise hash={hash} recommendation={} applied={} -->",
        recommendation.as_str(),
        applied.as_str(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdviseMarker {
    hash: String,
    /// A marker predating this field (v1 advise) parses as `Proposal` — the
    /// only action v1 ever took.
    applied: AppliedLevel,
    /// The recommendation this marker recorded. `None` for a malformed or
    /// future value; used to decide whether an unchanged proposal is still
    /// promotable under the current `apply` (so a proposal kind outside `apply`
    /// isn't re-scanned forever).
    recommendation: Option<Recommendation>,
}

fn parse_advise_marker(comment: &str) -> Option<AdviseMarker> {
    let rest = comment.split("<!-- meguri:triage-advise ").nth(1)?;
    let fields = rest.split("-->").next()?;
    let mut hash = None;
    let mut applied = AppliedLevel::Proposal;
    let mut recommendation = None;
    for part in fields.split_whitespace() {
        if let Some(v) = part.strip_prefix("hash=") {
            hash = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("applied=") {
            applied = match v {
                "real" => AppliedLevel::Real,
                "declined" => AppliedLevel::Declined,
                _ => AppliedLevel::Proposal,
            };
        } else if let Some(v) = part.strip_prefix("recommendation=") {
            recommendation = Recommendation::from_str(v);
        }
    }
    Some(AdviseMarker {
        hash: hash?,
        applied,
        recommendation,
    })
}

/// The most recent advise marker on `issue`, if any — last-wins, since only
/// the latest action's hash and level matter for idempotency.
async fn latest_advise_marker(deps: &Deps, issue: i64) -> Result<Option<AdviseMarker>> {
    let comments = deps.forge().issue_comments(issue).await?;
    Ok(comments.iter().rev().find_map(|c| parse_advise_marker(c)))
}

/// The evidence comment posted alongside a proposal label: the hidden marker
/// first, then the human-readable rationale (confidence / complexity /
/// rationale / missing info) and how to adopt or reject it.
fn render_advise_comment(item: &TriageItem, hash: &str) -> String {
    let label = proposal_label(item.recommendation).unwrap_or_default();
    let mut body = format!(
        "{marker}\n🔀 **meguri triage 提案** — `{label}` を提案します(確信度 {confidence:.2}、\
         複雑度: {complexity})。\n\n{rationale}\n",
        marker = advise_marker(hash, item.recommendation, AppliedLevel::Proposal),
        confidence = item.confidence,
        complexity = item.estimated_complexity.as_str(),
        rationale = item.rationale,
    );
    if let Some(mi) = &item.missing_info
        && !mi.is_empty()
    {
        body.push_str(&format!("\n⚠️ 要確認: {mi}\n"));
    }
    body.push_str(&format!(
        "\n---\n採用する場合はこの issue に `{real}` を人間の手で付与してください(meguri は\
         自動昇格しません)。却下する場合は `{label}` ラベルを外すだけで構いません — 内容が変わる\
         まで再提案しません。誤検知が続くなら `triage.ignore` へどうぞ。\n",
        real = real_label(item.recommendation).unwrap_or_default(),
    ));
    body
}

/// The reason comment posted alongside an `auto` promotion: the hidden marker
/// (recorded at `real` level), the human-readable rationale, and how to revert.
/// Reversibility (ADR 0017): a human removes the label to roll it back.
fn render_promote_comment(item: &TriageItem, label: &str, hash: &str) -> String {
    let mut body = format!(
        "{marker}\n🔀 **meguri triage 自動昇格** — `{label}` を付与しました(確信度 {confidence:.2}、\
         複雑度: {complexity})。既存の worker / planner ループが着手します。\n\n{rationale}\n",
        marker = advise_marker(hash, item.recommendation, AppliedLevel::Real),
        confidence = item.confidence,
        complexity = item.estimated_complexity.as_str(),
        rationale = item.rationale,
    );
    if let Some(mi) = &item.missing_info
        && !mi.is_empty()
    {
        body.push_str(&format!("\n⚠️ 要確認: {mi}\n"));
    }
    body.push_str(&format!(
        "\n---\n差し戻すには `{label}` ラベルを外してください — 内容が変わるまで再昇格しません\
         (worker が既に着手していれば run を止めるか `{hold}` を使ってください)。誤検知が続くなら \
         `triage.ignore` へどうぞ。\n",
        hold = forge::LABEL_HOLD,
    ));
    body
}

/// Record that `auto` evaluated an issue that already carried a triage marker
/// (a pending proposal, or a stale-hash marker whose content changed) and chose
/// not to promote it — because the recommendation is not a promotable kind in
/// `apply`, is below `confidence_threshold`, or is silenced by `triage.ignore`.
/// The `applied=declined` marker at the current content lets the next sweep
/// treat the decision as settled and stop re-triaging until the content changes.
/// Best-effort — a failed write just means the next interval retries. Any
/// proposal label is left in place (a human can still promote by hand); this
/// only silences auto's own re-evaluation.
async fn record_decline(deps: &Deps, item: &TriageItem, hash: &str) {
    if let Err(e) = deps
        .forge()
        .comment(item.issue, &render_decline_comment(item, hash))
        .await
    {
        tracing::warn!("triage auto-decline note on issue #{}: {e:#}", item.issue);
    }
}

/// The note recorded when `auto` declines an issue: a hidden `applied=declined`
/// marker over the current content, plus a short reason.
fn render_decline_comment(item: &TriageItem, hash: &str) -> String {
    format!(
        "{marker}\n🔀 **meguri triage** — この内容では auto は本ラベルを付けません(`apply` 対象外、\
         確信度不足、または `triage.ignore` 該当)。内容が変われば再評価します。人手での昇格は引き続き \
         可能です。\n",
        marker = advise_marker(hash, item.recommendation, AppliedLevel::Declined),
    )
}

fn ignored(patterns: &[String], haystacks: &[&str]) -> bool {
    patterns
        .iter()
        .filter(|p| !p.is_empty())
        .any(|p| haystacks.iter().any(|h| h.contains(p.as_str())))
}

/// The full report body: marker first, then a human-readable snapshot table.
/// The ignore list is applied here, at render time, so adding a pattern makes
/// the row vanish on the next sweep. Rendered even with zero recommendations —
/// the marker must advance or the same state would be swept again.
#[allow(clippy::too_many_arguments)]
pub fn render_report(
    head: &str,
    scanned: u64,
    scanned_display: &str,
    max_issue: i64,
    items: &[TriageItem],
    ignore: &[String],
    mode: TriageMode,
    backlog: bool,
    promoted: &[i64],
) -> String {
    let mut items: Vec<&TriageItem> = items
        .iter()
        .filter(|it| {
            !ignored(
                ignore,
                &[
                    &format!("#{}", it.issue),
                    it.recommendation.as_str(),
                    &it.rationale,
                    it.missing_info.as_deref().unwrap_or(""),
                ],
            )
        })
        .collect();
    items.sort_by_key(|it| it.issue);

    let short = head.get(..12).unwrap_or(head);
    let mut body = format!(
        "{marker}\n🔀 **meguri triage report** — recommendations for the \
         untriaged open issues at `{short}`, swept {scanned_display}. Issues \
         that stop being untriaged (you labeled them, or they closed) drop off \
         on the next sweep.\n",
        marker = triage_marker(head, scanned, max_issue, backlog),
    );
    if backlog {
        body.push_str(
            "\n⏳ `max_actions_per_tick` を使い切ったため、一部の提案は次回スイープへ持ち越し \
             です。\n",
        );
    }

    if items.is_empty() {
        body.push_str("\n_No open issues to triage._\n");
    } else {
        body.push_str("\n| Issue | 推薦 | 確信度 | 複雑度 | 根拠 |\n|---|---|---|---|---|\n");
        for it in &items {
            let mut rationale = it.rationale.replace('\n', " ");
            if let Some(mi) = &it.missing_info
                && !mi.is_empty()
            {
                rationale.push_str(&format!("<br>⚠️ 要確認: {}", mi.replace('\n', " ")));
            }
            // In `auto`, mark the rows this sweep actually promoted to a real
            // label (reversibility: the reader sees exactly what was auto-started).
            if promoted.contains(&it.issue) {
                rationale.push_str(&format!(
                    "<br>✅ 昇格: `{}` 付与",
                    real_label(it.recommendation).unwrap_or_default()
                ));
            }
            body.push_str(&format!(
                "| #{} | {} | {:.2} | {} | {} |\n",
                it.issue,
                it.recommendation.as_str(),
                it.confidence,
                it.estimated_complexity.as_str(),
                rationale,
            ));
        }
    }

    if mode == TriageMode::Auto {
        body.push_str(&format!(
            "\n---\n**auto モード**: `apply` に含まれ確信度が `confidence_threshold` 以上の \
             `ready` / `plan` 推薦は、対象 issue に本ラベル(`{ready}` / `{plan}`)を直接付与し、\
             理由コメントを残します(上表の ✅ 昇格 行)。既存の worker / planner ループが着手します。\
             差し戻すには本ラベルを外してください — 内容が変わるまで再昇格しません(着手済みなら run を \
             止めるか `{hold}`)。閾値未満・`apply` 外・`needs-human` / `skip` / `hold` は据え置きで、\
             ここに載るだけです。誤検知は `triage.ignore`(この本文の編集は毎スイープ上書きされます)、\
             スイープ停止は `{hold}` をこの issue に。\n",
            ready = forge::LABEL_READY,
            plan = forge::LABEL_PLAN,
            hold = forge::LABEL_HOLD,
        ));
    } else if mode == TriageMode::Advise {
        body.push_str(&format!(
            "\n---\n`ready` / `plan` / `needs-human` recommendations above also get a \
             proposal label (`meguri:triage-*`) and an evidence comment directly on \
             that issue — promote a proposal by applying `{ready}` / `{plan}` / \
             `{needs_human}` yourself, or reject it by removing the proposal label \
             (meguri won't re-propose it until the issue's content changes). To \
             silence a bad recommendation entirely, add a substring pattern to \
             `triage.ignore` (editing this body doesn't stick; it is rewritten every \
             sweep). Pause the sweep with `{hold}` on this issue.\n",
            ready = forge::LABEL_READY,
            plan = forge::LABEL_PLAN,
            needs_human = forge::LABEL_NEEDS_HUMAN,
            hold = forge::LABEL_HOLD,
        ));
    } else {
        body.push_str(&format!(
            "\n---\nTo adopt a recommendation, apply `{ready}` / `{plan}` to that \
             issue yourself — the existing loops take it from there (triage never \
             labels or comments on issues in `report` mode). To silence a bad \
             recommendation, add a substring pattern to `triage.ignore` in the \
             meguri config (editing this body doesn't stick; it is rewritten every \
             sweep). Pause the sweep with `{hold}` on this issue.\n",
            ready = forge::LABEL_READY,
            plan = forge::LABEL_PLAN,
            hold = forge::LABEL_HOLD,
        ));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_roundtrip_including_none() {
        let m = triage_marker("abc123", 1_700_000_000, 42, false);
        let parsed = parse_triage_marker(&format!("{m}\nreport body")).unwrap();
        assert_eq!(parsed.head, "abc123");
        assert_eq!(parsed.scanned, 1_700_000_000);
        assert_eq!(parsed.max_issue, 42);
        assert!(!parsed.backlog);

        let backlogged = triage_marker("abc123", 1_700_000_000, 42, true);
        assert!(parse_triage_marker(&backlogged).unwrap().backlog);

        let none = triage_marker(MARKER_HEAD_NONE, 7, 0, false);
        let parsed = parse_triage_marker(&none).unwrap();
        assert_eq!(parsed.head, MARKER_HEAD_NONE);
        assert_eq!(parsed.scanned, 7);
        assert_eq!(parsed.max_issue, 0);

        assert_eq!(parse_triage_marker("no marker here"), None);
        // A marker missing max_issue is rejected (head/scanned/max_issue are
        // required; backlog alone is optional for pre-#87-followup markers).
        assert_eq!(
            parse_triage_marker("<!-- meguri:triage head=x scanned=1 -->"),
            None
        );
        // A marker predating the backlog field parses as backlog=false.
        let old =
            parse_triage_marker("<!-- meguri:triage head=x scanned=1 max_issue=2 -->").unwrap();
        assert!(!old.backlog);
    }

    #[test]
    fn needs_scan_decision_table() {
        let day = 86_400;
        let m = |head: &str, scanned: u64, max_issue: i64| TriageMarker {
            head: head.into(),
            scanned,
            max_issue,
            backlog: false,
        };
        let mb = |head: &str, scanned: u64, max_issue: i64, backlog: bool| TriageMarker {
            head: head.into(),
            scanned,
            max_issue,
            backlog,
        };
        // No marker: always scan.
        assert!(needs_triage_scan(None, "abc", 5, 1000, day, false));
        // Same head, no new issue, no advise drift: never rescan, however
        // much time passed.
        assert!(!needs_triage_scan(
            Some(&m("abc", 0, 5)),
            "abc",
            5,
            10 * day,
            day,
            false,
        ));
        // Head moved but within the interval: wait.
        assert!(!needs_triage_scan(
            Some(&m("old", 1000, 5)),
            "abc",
            5,
            1000 + day - 1,
            day,
            false,
        ));
        // Head moved and the interval elapsed: scan.
        assert!(needs_triage_scan(
            Some(&m("old", 1000, 5)),
            "abc",
            5,
            1000 + day,
            day,
            false,
        ));
        // Same head but a new issue appeared (max_issue grew) after the
        // interval: scan — the new-issue signal, head-independent.
        assert!(needs_triage_scan(
            Some(&m("abc", 1000, 5)),
            "abc",
            6,
            1000 + day,
            day,
            false,
        ));
        // New issue but still within the interval: wait (interval rate-limits
        // every signal).
        assert!(!needs_triage_scan(
            Some(&m("abc", 1000, 5)),
            "abc",
            6,
            1000 + day - 1,
            day,
            false,
        ));
        // Initializing marker `head=none max_issue=0` behaves like a moved
        // head: rescans once the interval elapses, not before.
        assert!(!needs_triage_scan(
            Some(&m(MARKER_HEAD_NONE, 1000, 0)),
            "abc",
            0,
            1500,
            day,
            false,
        ));
        assert!(needs_triage_scan(
            Some(&m(MARKER_HEAD_NONE, 1000, 0)),
            "abc",
            0,
            1000 + day,
            day,
            false,
        ));
        // advise content drift, head/new-issue signals both quiet: scan once
        // the interval elapsed, just like the other two signals — and not
        // before.
        assert!(!needs_triage_scan(
            Some(&m("abc", 1000, 5)),
            "abc",
            5,
            1000 + day - 1,
            day,
            true,
        ));
        assert!(needs_triage_scan(
            Some(&m("abc", 1000, 5)),
            "abc",
            5,
            1000 + day,
            day,
            true,
        ));
        // marker.backlog alone (last sweep hit max_actions_per_tick), head
        // and max_issue both still, drift signal quiet: still scans once the
        // interval elapsed — the third independent trigger.
        assert!(!needs_triage_scan(
            Some(&mb("abc", 1000, 5, true)),
            "abc",
            5,
            1000 + day - 1,
            day,
            false,
        ));
        assert!(needs_triage_scan(
            Some(&mb("abc", 1000, 5, true)),
            "abc",
            5,
            1000 + day,
            day,
            false,
        ));
        // backlog=false alongside otherwise-identical state: back to quiet.
        assert!(!needs_triage_scan(
            Some(&mb("abc", 1000, 5, false)),
            "abc",
            5,
            1000 + day,
            day,
            false,
        ));
    }

    #[test]
    fn replace_marker_swaps_in_place_or_prepends() {
        let body = format!("{}\nrecs", triage_marker("old", 1, 3, false));
        let updated = replace_marker(&body, &triage_marker("old", 99, 3, false));
        assert_eq!(parse_triage_marker(&updated).unwrap().scanned, 99);
        assert!(updated.contains("recs"));
        assert_eq!(updated.matches("meguri:triage").count(), 1);

        let updated = replace_marker("plain body", &triage_marker("h", 2, 0, false));
        assert!(updated.starts_with(&triage_marker("h", 2, 0, false)));
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

        std::fs::write(&path, r#"{"recommendations": []}"#).unwrap();
        assert!(read_report(dir.path()).unwrap().recommendations.is_empty());

        std::fs::write(
            &path,
            r#"{"recommendations": [{"issue": 81, "recommendation": "needs-human",
                "confidence": 0.4, "estimated_complexity": "large",
                "rationale": "requirements unclear"}]}"#,
        )
        .unwrap();
        let report = read_report(dir.path()).unwrap();
        assert_eq!(report.recommendations.len(), 1);
        let it = &report.recommendations[0];
        assert_eq!(it.issue, 81);
        assert_eq!(it.recommendation, Recommendation::NeedsHuman);
        assert_eq!(it.estimated_complexity, Complexity::Large);
        assert_eq!(it.missing_info, None);
    }

    #[test]
    fn prompt_lists_issues_and_demands_report_not_changes() {
        let candidates = vec![
            Issue {
                number: 81,
                title: "add caching".into(),
                body: "we should cache X".into(),
                labels: vec![],
            },
            Issue {
                number: 90,
                title: "rework auth".into(),
                body: "big and vague".into(),
                labels: vec![],
            },
        ];
        let prompt = execute_prompt(&candidates, "deadbeef", None);
        assert!(prompt.contains("Issue #81: add caching"));
        assert!(prompt.contains("we should cache X"));
        assert!(prompt.contains("Issue #90: rework auth"));
        assert!(prompt.contains(REPORT_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("deadbeef"));
        assert!(!prompt.contains("# Output language"));

        let prompt = execute_prompt(&candidates, "deadbeef", Some("日本語"));
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    #[test]
    fn coverage_flags_missing_duplicate_and_out_of_scope() {
        let candidate = |n: i64| Issue {
            number: n,
            title: "t".into(),
            body: String::new(),
            labels: vec![],
        };
        let candidates = vec![candidate(60), candidate(70)];
        let rec = |n: i64| TriageItem {
            issue: n,
            recommendation: Recommendation::Ready,
            confidence: 0.5,
            estimated_complexity: Complexity::Small,
            rationale: "x".into(),
            missing_info: None,
        };

        // Exact cover: accepted.
        assert!(coverage_problem(&candidates, &[rec(60), rec(70)]).is_none());

        // A dropped issue is corrected (this is the case that would otherwise
        // let the marker advance over an un-triaged issue).
        let p = coverage_problem(&candidates, &[rec(60)]).unwrap();
        assert!(p.contains("no recommendation for issue(s) [70]"), "{p}");

        // An empty report with candidates present is "missing all".
        assert!(coverage_problem(&candidates, &[]).is_some());

        // A duplicate is corrected.
        let p = coverage_problem(&candidates, &[rec(60), rec(60), rec(70)]).unwrap();
        assert!(p.contains("more than one"), "{p}");

        // An out-of-scope issue number is corrected.
        let p = coverage_problem(&candidates, &[rec(60), rec(70), rec(99)]).unwrap();
        assert!(p.contains("[99]"), "{p}");
    }

    fn sample_items() -> Vec<TriageItem> {
        vec![
            TriageItem {
                issue: 81,
                recommendation: Recommendation::Ready,
                confidence: 0.82,
                estimated_complexity: Complexity::Small,
                rationale: "clear scope, one file".into(),
                missing_info: None,
            },
            TriageItem {
                issue: 90,
                recommendation: Recommendation::Plan,
                confidence: 0.5,
                estimated_complexity: Complexity::Large,
                rationale: "big and vague".into(),
                missing_info: Some("which auth backend?".into()),
            },
        ]
    }

    #[test]
    fn report_renders_table_and_marker() {
        let body = render_report(
            "0123456789abcdef",
            1_700_000_000,
            "2026-07-14T00:00:00Z",
            90,
            &sample_items(),
            &[],
            TriageMode::Report,
            false,
            &[],
        );
        assert!(body.starts_with(&triage_marker("0123456789abcdef", 1_700_000_000, 90, false)));
        assert!(body.contains("`0123456789ab`"), "{body}");
        assert!(body.contains("| Issue | 推薦 | 確信度 | 複雑度 | 根拠 |"));
        assert!(body.contains("| #81 | ready | 0.82 | small | clear scope, one file |"));
        assert!(body.contains("| #90 | plan | 0.50 | large |"));
        assert!(body.contains("⚠️ 要確認: which auth backend?"));
        assert!(body.contains("triage.ignore"));
    }

    #[test]
    fn advise_mode_footer_mentions_proposal_labels() {
        let body = render_report(
            "head",
            1,
            "now",
            90,
            &sample_items(),
            &[],
            TriageMode::Advise,
            false,
            &[],
        );
        assert!(body.contains("meguri:triage-"));
        assert!(body.contains("triage.ignore"));
        assert!(!body.contains("max_actions_per_tick"));
    }

    #[test]
    fn backlog_marker_and_footer_note_survive_render() {
        let body = render_report(
            "head",
            1,
            "now",
            90,
            &sample_items(),
            &[],
            TriageMode::Advise,
            true,
            &[],
        );
        assert!(parse_triage_marker(&body).unwrap().backlog);
        assert!(body.contains("max_actions_per_tick"), "{body}");
    }

    #[test]
    fn ignore_list_drops_rows() {
        let body = render_report(
            "head",
            1,
            "now",
            90,
            &sample_items(),
            &["#81".to_string()],
            TriageMode::Report,
            false,
            &[],
        );
        assert!(!body.contains("| #81 |"));
        assert!(body.contains("| #90 |"));

        // A rationale/missing_info substring match works too.
        let body = render_report(
            "head",
            1,
            "now",
            90,
            &sample_items(),
            &["auth backend".to_string()],
            TriageMode::Report,
            false,
            &[],
        );
        assert!(!body.contains("| #90 |"));
        assert!(body.contains("| #81 |"));
    }

    #[test]
    fn empty_report_still_carries_the_marker() {
        let body = render_report("h", 7, "now", 0, &[], &[], TriageMode::Report, false, &[]);
        assert!(body.contains(&triage_marker("h", 7, 0, false)));
        assert!(body.contains("_No open issues to triage._"));
    }

    #[test]
    fn advise_marker_roundtrip_and_content_hash_detects_changes() {
        let hash = content_hash(&Issue {
            number: 1,
            title: "t".into(),
            body: "b".into(),
            labels: vec![],
        });
        let marker = advise_marker(&hash, Recommendation::Ready, AppliedLevel::Proposal);
        let parsed = parse_advise_marker(&format!("{marker}\nsome rationale")).unwrap();
        assert_eq!(parsed.hash, hash);
        assert_eq!(parsed.applied, AppliedLevel::Proposal);
        assert_eq!(parsed.recommendation, Some(Recommendation::Ready));
        assert_eq!(parse_advise_marker("no marker here"), None);

        // A `real` marker roundtrips its level and recommendation.
        let real = advise_marker(&hash, Recommendation::Plan, AppliedLevel::Real);
        let parsed = parse_advise_marker(&real).unwrap();
        assert_eq!(parsed.applied, AppliedLevel::Real);
        assert_eq!(parsed.recommendation, Some(Recommendation::Plan));
        // A `declined` marker roundtrips too.
        let declined = advise_marker(&hash, Recommendation::Ready, AppliedLevel::Declined);
        assert_eq!(
            parse_advise_marker(&declined).unwrap().applied,
            AppliedLevel::Declined
        );
        // A marker predating the `applied` field parses as `proposal` (v1's
        // only action level), for backward compatibility, keeping its recommendation.
        let old = format!("<!-- meguri:triage-advise hash={hash} recommendation=plan -->");
        let parsed = parse_advise_marker(&old).unwrap();
        assert_eq!(parsed.applied, AppliedLevel::Proposal);
        assert_eq!(parsed.recommendation, Some(Recommendation::Plan));
        // A malformed/unknown recommendation is tolerated as `None`.
        let bad = format!("<!-- meguri:triage-advise hash={hash} recommendation=bogus -->");
        assert_eq!(parse_advise_marker(&bad).unwrap().recommendation, None);

        let changed_hash = content_hash(&Issue {
            number: 1,
            title: "t".into(),
            body: "different body".into(),
            labels: vec![],
        });
        assert_ne!(hash, changed_hash);
    }

    #[test]
    fn advise_comment_carries_marker_and_adoption_instructions() {
        let item = TriageItem {
            issue: 81,
            recommendation: Recommendation::Ready,
            confidence: 0.82,
            estimated_complexity: Complexity::Small,
            rationale: "clear scope, one file".into(),
            missing_info: Some("confirm the target module".into()),
        };
        let body = render_advise_comment(&item, "deadbeef");
        assert!(body.starts_with("<!-- meguri:triage-advise hash=deadbeef"));
        assert!(body.contains("meguri:triage-ready"));
        assert!(body.contains("clear scope, one file"));
        assert!(body.contains("⚠️ 要確認: confirm the target module"));
        assert!(body.contains(forge::LABEL_READY));
    }

    #[test]
    fn is_engaged_label_excludes_only_proposal_labels() {
        assert!(is_engaged_label(forge::LABEL_READY));
        assert!(is_engaged_label(forge::LABEL_HOLD));
        assert!(!is_engaged_label(forge::LABEL_TRIAGE_READY));
        assert!(!is_engaged_label(forge::LABEL_TRIAGE_PLAN));
        assert!(!is_engaged_label(forge::LABEL_TRIAGE_NEEDS_HUMAN));
        assert!(!is_engaged_label("not-a-meguri-label"));
    }

    #[test]
    fn promote_label_honors_apply_and_two_axis_model() {
        use TriageAction::{Plan, Ready};
        // Only recommendations listed in `apply` promote, and only ready/plan.
        assert_eq!(
            promote_label(Recommendation::Ready, &[Ready]),
            Some(forge::LABEL_READY)
        );
        assert_eq!(promote_label(Recommendation::Plan, &[Ready]), None); // not in apply
        assert_eq!(
            promote_label(Recommendation::Plan, &[Ready, Plan]),
            Some(forge::LABEL_PLAN)
        );
        // needs-human is a ball label (ADR 0005), never auto-promoted; hold/skip
        // have no real label. None regardless of `apply`.
        assert_eq!(
            promote_label(Recommendation::NeedsHuman, &[Ready, Plan]),
            None
        );
        assert_eq!(promote_label(Recommendation::Hold, &[Ready, Plan]), None);
        assert_eq!(promote_label(Recommendation::Skip, &[Ready, Plan]), None);
        // Empty apply promotes nothing.
        assert_eq!(promote_label(Recommendation::Ready, &[]), None);
    }

    #[test]
    fn auto_footer_mentions_real_labels_and_rollback() {
        let body = render_report(
            "head",
            1,
            "now",
            90,
            &sample_items(),
            &[],
            TriageMode::Auto,
            false,
            &[81],
        );
        // Real labels, not proposal labels, and how to revert.
        assert!(body.contains(forge::LABEL_READY));
        assert!(body.contains(forge::LABEL_PLAN));
        assert!(!body.contains("meguri:triage-"));
        assert!(body.contains("差し戻す"));
        assert!(body.contains("triage.ignore"));
        // The promoted row is marked with the label it received.
        assert!(body.contains("✅ 昇格"), "{body}");
    }

    #[test]
    fn promote_comment_carries_real_marker_and_rollback() {
        let item = TriageItem {
            issue: 81,
            recommendation: Recommendation::Ready,
            confidence: 0.9,
            estimated_complexity: Complexity::Small,
            rationale: "clear scope".into(),
            missing_info: None,
        };
        let body = render_promote_comment(&item, forge::LABEL_READY, "deadbeef");
        assert!(body.starts_with("<!-- meguri:triage-advise hash=deadbeef"));
        assert!(body.contains("applied=real"), "{body}");
        assert!(body.contains(forge::LABEL_READY));
        assert!(body.contains("clear scope"));
        assert!(body.contains("差し戻す"));
        // Its marker parses back as a `real` promotion.
        assert_eq!(
            parse_advise_marker(&body).unwrap().applied,
            AppliedLevel::Real
        );
    }
}
