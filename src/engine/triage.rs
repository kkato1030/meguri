//! The triage loop: a read-only detector that periodically looks at every
//! untriaged open issue and rewrites a single per-project report issue
//! (`meguri:triage-report`) with a recommendation for each — how meguri
//! should handle it (`ready` / `plan` / `needs-human` / `hold` / `skip`),
//! how confident it is, and how big the work looks.
//!
//! It is the cleaner's twin one step further in (ADR 0006): the cleaner
//! automates *observation* (code/issue divergence), triage automates a
//! *decision* (what to do with an issue). So it stays read-only in v0 — its
//! write boundary is exactly that one report issue: no pushes, no branch
//! operations, and (unlike v1/v2) no labels or comments on the triaged issues
//! themselves. Humans act on the report: adopt a recommendation by applying
//! `meguri:ready`/`meguri:plan` yourself, silence a bad one via
//! `triage.ignore`, pause the sweep with `meguri:hold` on the report issue.
//!
//! Opt-in: `[triage] mode` defaults to `off`; the loop only sweeps on
//! `report`. Re-scan is rate-limited like the cleaner (default-branch head +
//! interval) but adds a new-issue signal — an open issue numbered above the
//! last scan's max triggers a fresh sweep even while the head is still, so a
//! new issue is triaged without waiting for the next push (ADR 0006).
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
use crate::config::TriageMode;
use crate::forge::{self, Issue};
use crate::gitops;
use crate::store::{ROLE_AUTHOR, RunRecord, RunStatus};
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
/// covers, when the last sweep (or failed attempt) ran, and the largest open
/// issue number seen at that time (the new-issue signal). The issue body is
/// the durable scan state — nothing is kept locally ("Authority").
pub fn triage_marker(head: &str, scanned: u64, max_issue: i64) -> String {
    format!("<!-- meguri:triage head={head} scanned={scanned} max_issue={max_issue} -->")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageMarker {
    pub head: String,
    /// Unix epoch seconds of the last sweep attempt.
    pub scanned: u64,
    /// Largest open issue number at the last sweep (drives the new-issue
    /// signal: any open issue above this re-triggers a sweep).
    pub max_issue: i64,
}

pub fn parse_triage_marker(body: &str) -> Option<TriageMarker> {
    let rest = body.split("<!-- meguri:triage ").nth(1)?;
    let fields = rest.split("-->").next()?;
    let mut head = None;
    let mut scanned = None;
    let mut max_issue = None;
    for part in fields.split_whitespace() {
        if let Some(v) = part.strip_prefix("head=") {
            head = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("scanned=") {
            scanned = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("max_issue=") {
            max_issue = v.parse().ok();
        }
    }
    Some(TriageMarker {
        head: head?,
        scanned: scanned?,
        max_issue: max_issue?,
    })
}

/// The discovery decision, pure so it unit-tests without a forge. Sweep when
/// nothing was ever recorded, or when *something changed* — the head moved or
/// an open issue numbered above the last scan appeared — **and** the interval
/// elapsed. The interval rate-limits every signal, so a failed sweep that only
/// advances `scanned` still paces the retry. A truly unchanged state (same
/// head, no new issue) is never re-swept, however old.
pub fn needs_triage_scan(
    marker: Option<&TriageMarker>,
    current_head: &str,
    max_open_issue: i64,
    now: u64,
    interval_secs: u64,
) -> bool {
    match marker {
        None => true,
        Some(m) => {
            let changed = m.head != current_head || max_open_issue > m.max_issue;
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
    /// Gated on `[triage] mode == report`: the loop is opt-in, so it is a
    /// no-op until a human turns it on.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.config.triage_for(&deps.project).mode != TriageMode::Report {
            return Ok(Vec::new());
        }
        let issues = deps
            .forge
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
            gitops::default_branch_head(&deps.project.repo_path, &deps.project.default_branch)
                .await?;
        let max_open = max_open_issue(deps).await?;
        let marker = report.as_ref().and_then(|i| parse_triage_marker(&i.body));
        let interval = deps.config.triage_for(&deps.project).interval_hours * 3600;
        if !needs_triage_scan(marker.as_ref(), &head, max_open, epoch_now(), interval) {
            return Ok(Vec::new());
        }
        Ok(vec![Target {
            issue_number: report.map(|i| i.number).unwrap_or(0),
            title: REPORT_TITLE.to_string(),
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
        .forge
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
                    if let Some(pane_id) = &run.mux_pane_id {
                        let _ = deps
                            .mux
                            .kill_pane(&crate::mux::PaneId(pane_id.clone()))
                            .await;
                    }
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
                    ROLE_AUTHOR,
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
        super::reaper::release_pane(deps, run.issue_number, ROLE_AUTHOR, "triage sweep finished")
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
    if let Err(e) = gitops::remove_worktree(&deps.project.repo_path, worktree).await {
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
            .forge
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
        let issue = deps.forge.get_issue(run.issue_number).await?;
        if issue.has_label(forge::LABEL_HOLD) {
            return Ok(Prepared::Skip(format!(
                "report issue #{} is on hold ({})",
                issue.number,
                forge::LABEL_HOLD
            )));
        }
        parse_triage_marker(&issue.body)
    };

    let head =
        gitops::default_branch_head(&deps.project.repo_path, &deps.project.default_branch).await?;
    let max_open = max_open_issue(deps).await?;
    let interval = deps.config.triage_for(&deps.project).interval_hours * 3600;
    if !needs_triage_scan(marker.as_ref(), &head, max_open, epoch_now(), interval) {
        return Ok(Prepared::Skip(format!(
            "head {head} needs no sweep (already scanned, within interval, or no new issue)"
        )));
    }

    cp.report_issue = run.issue_number;
    cp.head_sha = head;
    cp.prev_head = marker.as_ref().map(|m| m.head.clone()).unwrap_or_default();
    cp.prev_max_issue = marker.as_ref().map(|m| m.max_issue).unwrap_or(0);
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
        &deps.project.repo_path,
        &wt,
        &deps.project.default_branch,
        &cp.head_sha,
    )
    .await?;
    deps.store
        .update_run_worktree(&run.id, &deps.project.default_branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.created",
        json!({ "head": cp.head_sha, "path": wt.to_string_lossy() }),
    )?;
    Ok(())
}

/// The untriaged open issues this sweep considers: open, no `meguri:` label
/// (so not held and not otherwise engaged), no unresolved blocker. Sorted by
/// number for a stable prompt and report.
async fn gather_candidates(deps: &Deps) -> Result<Vec<Issue>> {
    let mut candidates = Vec::new();
    for issue in deps.forge.list_open_issues().await? {
        if issue
            .labels
            .iter()
            .any(|l| l.starts_with(WORKFLOW_LABEL_PREFIX))
        {
            continue;
        }
        if flow::has_unresolved_blockers(deps, issue.number).await {
            continue;
        }
        candidates.push(issue);
    }
    candidates.sort_by_key(|i| i.number);
    Ok(candidates)
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
    let marker = triage_marker(prev_head, epoch_now(), cp.prev_max_issue);
    if cp.report_issue == 0 {
        let body = format!(
            "{marker}\n🔀 **meguri triage report** — initializing: the first \
             sweep did not complete. meguri retries after the configured \
             interval (`triage.interval_hours`)."
        );
        match deps
            .forge
            .create_issue(REPORT_TITLE, &body, &[forge::LABEL_TRIAGE_REPORT])
            .await
        {
            Ok(number) => {
                let _ = deps.store.emit(
                    Some(&run.id),
                    "triage.report_created",
                    json!({ "issue": number, "initializing": true }),
                );
            }
            Err(e) => tracing::warn!("cannot create initializing triage report issue: {e:#}"),
        }
        return;
    }
    let body = match deps.forge.get_issue(cp.report_issue).await {
        Ok(issue) => replace_marker(&issue.body, &marker),
        Err(e) => {
            tracing::warn!(
                "cannot re-read triage report issue #{}: {e:#}",
                cp.report_issue
            );
            return;
        }
    };
    if let Err(e) = deps.forge.update_issue_body(cp.report_issue, &body).await {
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

/// settle: render the snapshot body and write the report issue — the loop's
/// only forge write. `max_issue` is recorded fresh here (so the just-created
/// report issue does not re-trigger the next sweep). Returns the issue number.
async fn settle(deps: &Deps, run: &RunRecord, cp: &TriageCheckpoint) -> Result<i64> {
    let ignore = deps.config.triage_for(&deps.project).ignore.clone();
    let max_open = max_open_issue(deps).await.unwrap_or(cp.prev_max_issue);
    let body = render_report(
        &cp.head_sha,
        epoch_now(),
        &crate::store::now(),
        max_open,
        &cp.recommendations,
        &ignore,
    );

    let issue = if cp.report_issue == 0 {
        let number = deps
            .forge
            .create_issue(REPORT_TITLE, &body, &[forge::LABEL_TRIAGE_REPORT])
            .await?;
        deps.store.emit(
            Some(&run.id),
            "triage.report_created",
            json!({ "issue": number }),
        )?;
        number
    } else {
        deps.forge.update_issue_body(cp.report_issue, &body).await?;
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
        }),
    )?;
    Ok(issue)
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
pub fn render_report(
    head: &str,
    scanned: u64,
    scanned_display: &str,
    max_issue: i64,
    items: &[TriageItem],
    ignore: &[String],
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
        marker = triage_marker(head, scanned, max_issue),
    );

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

    body.push_str(&format!(
        "\n---\nTo adopt a recommendation, apply `{ready}` / `{plan}` to that \
         issue yourself — the existing loops take it from there (v0 triage \
         never labels or comments on issues). To silence a bad recommendation, \
         add a substring pattern to `triage.ignore` in the meguri config \
         (editing this body doesn't stick; it is rewritten every sweep). Pause \
         the sweep with `{hold}` on this issue.\n",
        ready = forge::LABEL_READY,
        plan = forge::LABEL_PLAN,
        hold = forge::LABEL_HOLD,
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_roundtrip_including_none() {
        let m = triage_marker("abc123", 1_700_000_000, 42);
        let parsed = parse_triage_marker(&format!("{m}\nreport body")).unwrap();
        assert_eq!(parsed.head, "abc123");
        assert_eq!(parsed.scanned, 1_700_000_000);
        assert_eq!(parsed.max_issue, 42);

        let none = triage_marker(MARKER_HEAD_NONE, 7, 0);
        let parsed = parse_triage_marker(&none).unwrap();
        assert_eq!(parsed.head, MARKER_HEAD_NONE);
        assert_eq!(parsed.scanned, 7);
        assert_eq!(parsed.max_issue, 0);

        assert_eq!(parse_triage_marker("no marker here"), None);
        // A marker missing max_issue is rejected (all three fields required).
        assert_eq!(
            parse_triage_marker("<!-- meguri:triage head=x scanned=1 -->"),
            None
        );
    }

    #[test]
    fn needs_scan_decision_table() {
        let day = 86_400;
        let m = |head: &str, scanned: u64, max_issue: i64| TriageMarker {
            head: head.into(),
            scanned,
            max_issue,
        };
        // No marker: always scan.
        assert!(needs_triage_scan(None, "abc", 5, 1000, day));
        // Same head, no new issue: never rescan, however much time passed.
        assert!(!needs_triage_scan(
            Some(&m("abc", 0, 5)),
            "abc",
            5,
            10 * day,
            day
        ));
        // Head moved but within the interval: wait.
        assert!(!needs_triage_scan(
            Some(&m("old", 1000, 5)),
            "abc",
            5,
            1000 + day - 1,
            day
        ));
        // Head moved and the interval elapsed: scan.
        assert!(needs_triage_scan(
            Some(&m("old", 1000, 5)),
            "abc",
            5,
            1000 + day,
            day
        ));
        // Same head but a new issue appeared (max_issue grew) after the
        // interval: scan — the new-issue signal, head-independent.
        assert!(needs_triage_scan(
            Some(&m("abc", 1000, 5)),
            "abc",
            6,
            1000 + day,
            day
        ));
        // New issue but still within the interval: wait (interval rate-limits
        // every signal).
        assert!(!needs_triage_scan(
            Some(&m("abc", 1000, 5)),
            "abc",
            6,
            1000 + day - 1,
            day
        ));
        // Initializing marker `head=none max_issue=0` behaves like a moved
        // head: rescans once the interval elapses, not before.
        assert!(!needs_triage_scan(
            Some(&m(MARKER_HEAD_NONE, 1000, 0)),
            "abc",
            0,
            1500,
            day
        ));
        assert!(needs_triage_scan(
            Some(&m(MARKER_HEAD_NONE, 1000, 0)),
            "abc",
            0,
            1000 + day,
            day
        ));
    }

    #[test]
    fn replace_marker_swaps_in_place_or_prepends() {
        let body = format!("{}\nrecs", triage_marker("old", 1, 3));
        let updated = replace_marker(&body, &triage_marker("old", 99, 3));
        assert_eq!(parse_triage_marker(&updated).unwrap().scanned, 99);
        assert!(updated.contains("recs"));
        assert_eq!(updated.matches("meguri:triage").count(), 1);

        let updated = replace_marker("plain body", &triage_marker("h", 2, 0));
        assert!(updated.starts_with(&triage_marker("h", 2, 0)));
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
        );
        assert!(body.starts_with(&triage_marker("0123456789abcdef", 1_700_000_000, 90)));
        assert!(body.contains("`0123456789ab`"), "{body}");
        assert!(body.contains("| Issue | 推薦 | 確信度 | 複雑度 | 根拠 |"));
        assert!(body.contains("| #81 | ready | 0.82 | small | clear scope, one file |"));
        assert!(body.contains("| #90 | plan | 0.50 | large |"));
        assert!(body.contains("⚠️ 要確認: which auth backend?"));
        assert!(body.contains("triage.ignore"));
    }

    #[test]
    fn ignore_list_drops_rows() {
        let body = render_report("head", 1, "now", 90, &sample_items(), &["#81".to_string()]);
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
        );
        assert!(!body.contains("| #90 |"));
        assert!(body.contains("| #81 |"));
    }

    #[test]
    fn empty_report_still_carries_the_marker() {
        let body = render_report("h", 7, "now", 0, &[], &[]);
        assert!(body.contains(&triage_marker("h", 7, 0)));
        assert!(body.contains("_No open issues to triage._"));
    }
}
