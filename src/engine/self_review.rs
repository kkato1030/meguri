//! The self-review phase: no longer a schedulable loop, but the worker's
//! **internal** self-review phase (ADR 0006). It runs inside the run's own
//! worktree, between `validate` and `open-pr`, and **never touches the
//! forge** — the review→fix ping-pong that used to travel as PR threads now
//! stays entirely local:
//!
//! 1. **review turn** — reads `git diff <base>...HEAD` locally (dropped at
//!    [`DIFF_FILE`]) in a separate `self-review` lane under the
//!    `self-reviewer` routing profile (model separation survives), and writes
//!    `{verdict, findings[]}` to [`REVIEW_FILE`]. `clean` ends the phase.
//! 2. **fix turn** — the author lane addresses the findings, declares a
//!    disposition (`fixed`/`waived`) per finding in [`FIX_FILE`], and commits;
//!    the project check is re-run; then back to a review turn.
//! 3. **findings ledger** (issue #212, ADR 0022) — findings are not a
//!    per-round snapshot but a cumulative ledger on the checkpoint: each entry
//!    carries a reviewer-confirmed `status` (open/fixed/waived). Convergence is
//!    "no open entry left". Round 2+ is not a fresh review — the reviewer only
//!    confirms whether prior findings are resolved and adds new blocking ones,
//!    reading the ledger plus an incremental diff since the last review.
//! 4. **escalation is behavioral, not a round count** (ADR 0022) — a
//!    `needs_human` verdict, a real ping-pong (a finding still open after two
//!    fix turns), or a disputed recorded decision escalate to a human. On the
//!    rounds cap with only minor blocking left, a single **final fix + validate**
//!    publishes instead of escalating; the last fix is not re-reviewed, and the
//!    PR footer records that.
//!
//! Findings ride the run's checkpoint in-memory; nothing is posted, so the
//! human's PR conversation stays a clean, human/external-review-only space,
//! and the fixer's discovery naturally narrows to human/external threads.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Deps;
use super::flow::{self, Checkpoint, Flavor, Kind, NeedsHuman};
use crate::gitops;
use crate::store::RunRecord;
use crate::turn::prompts::MEGURI_DIR;
use crate::turn::{TurnOutcome, TurnStatus};

/// Event kinds the self-review phase emits. `meguri stats review` (issue #213)
/// reads exactly these, so the emit sites and the measurement query share one
/// source of truth (a renamed event can't silently drop out of the stats).
/// [`EVENT_CLEAN`] / [`EVENT_UNCONVERGED`] / [`EVENT_NEEDS_HUMAN`] /
/// [`EVENT_PINGPONG`] / [`EVENT_FINAL_FIX`] are the **terminal** events — a phase
/// that ran the review machinery to a conclusion emits exactly one.
/// [`EVENT_CORRECTION`] is a mid-phase contract violation on a review turn (not
/// terminal).
pub const EVENT_CLEAN: &str = "self_review.clean";
pub const EVENT_UNCONVERGED: &str = "self_review.unconverged";
pub const EVENT_NEEDS_HUMAN: &str = "self_review.needs_human";
pub const EVENT_CORRECTION: &str = "self_review.correction";
/// A genuine ping-pong escalated: a finding still open after two fix turns
/// (issue #212, escalation reason 2).
pub const EVENT_PINGPONG: &str = "self_review.pingpong";
/// The cap→final-fix path published (issue #212): only minor blocking remained,
/// so a single final fix + validate ran and the PR opened with a footer noting
/// the last fix was not re-reviewed. Distinct from [`EVENT_UNCONVERGED`], which
/// now means a genuine escalation only.
pub const EVENT_FINAL_FIX: &str = "self_review.final_fix";

/// Where the orchestrator drops the local diff for the review turn to read
/// (worktree-relative; `.meguri/` is git-excluded, so it never dirties the
/// tree).
pub const DIFF_FILE: &str = ".meguri/self-review-diff.patch";
/// Where the orchestrator drops the incremental diff for a round 2+ review
/// (issue #212): what the fix turns changed since the previous review's HEAD.
pub const INCREMENTAL_DIFF_FILE: &str = ".meguri/self-review-incremental.patch";
/// Where the review turn writes its verdict + findings (worktree-relative).
pub const REVIEW_FILE: &str = ".meguri/self-review.json";
/// Where the fix turn writes its per-finding dispositions (issue #212).
pub const FIX_FILE: &str = ".meguri/self-review-fix.json";

/// The self-review disposition (issue #176, ADR 0012). Three-valued so the
/// reviewer itself classifies whether the diff can be repaired automatically
/// (`Fixable`, drives another fix round) or needs a person (`NeedsHuman`,
/// escalates at once). `Clean` publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// Nothing must change — publish.
    Clean,
    /// Something must change, and the agent can fix it — run another fix round.
    /// `findings` is accepted as a legacy alias (pre-#176 two-valued reviews).
    #[serde(alias = "findings")]
    Fixable,
    /// Something must change that needs a human judgment — escalate now.
    NeedsHuman,
}

/// What a finding is (issue #212, ADR 0022). No `severity` — every finding is
/// blocking by definition; non-blocking remarks stay in the `review` prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A bug/omission/convention miss the author fixes in code.
    #[default]
    Defect,
    /// An "A or B" the author must settle and record in the spec/impl; the next
    /// review only confirms it was recorded, never re-litigates the choice.
    Decision,
}

/// The reviewer-confirmed state of a ledger entry (issue #212). Distinct from
/// the author's [`Disposition`]: the author's claim never closes a finding — only
/// a review turn moves the status (omitting a finding = resolved; re-listing it =
/// still open).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Open,
    Fixed,
    Waived,
}

/// The author's latest claim about a finding on a fix turn (issue #212): they
/// either addressed it (`Fixed`) or disagree with it (`Waived`, reason required).
/// Recorded on the ledger as input to the next review; it does not itself change
/// the reviewer-confirmed [`FindingStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Fixed,
    Waived,
}

/// One finding from a review turn, anchored to a line on the NEW side of the
/// diff so the fix turn can locate it. Round 1 leaves `id` null (the
/// orchestrator assigns a stable id); round 2+ repeats an existing `id` to
/// re-list a still-unresolved finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Stable id across rounds (issue #212). `None` on a fresh finding — the
    /// orchestrator assigns it when it lands in the ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `defect` (fix in code) or `decision` (settle and record). Defaults to
    /// `defect` so a reviewer that omits it, and any pre-#212 checkpoint, still
    /// parse.
    #[serde(default)]
    pub kind: FindingKind,
    pub path: String,
    pub line: u64,
    pub body: String,
    /// Which review lens surfaced it (ADR 0008), if the reviewer tagged one.
    #[serde(default)]
    pub lens: Option<String>,
}

/// One cumulative ledger entry (issue #212, ADR 0022): a finding tracked across
/// rounds with its reviewer-confirmed status, the author's latest disposition,
/// and how many fix turns it has been through (the ping-pong counter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Stable id the orchestrator assigned (e.g. `f1`, `f2`).
    pub id: String,
    #[serde(default)]
    pub kind: FindingKind,
    pub path: String,
    pub line: u64,
    pub body: String,
    #[serde(default)]
    pub lens: Option<String>,
    /// Reviewer-confirmed status (omit=resolved, re-list=still open).
    pub status: FindingStatus,
    /// The author's latest claim (input to the next review, not the truth).
    #[serde(default)]
    pub author_disposition: Option<Disposition>,
    /// How many fix turns have written a disposition for this entry — a finding
    /// still open after two is a real ping-pong.
    #[serde(default)]
    pub fix_attempts: u32,
    /// The waive reason, or (for a `decision`) the decision content recorded.
    #[serde(default)]
    pub waive_reason: Option<String>,
    /// The round this finding was first raised.
    #[serde(default)]
    pub origin_round: u32,
}

/// One self-review round's outcome, for the PR-body `<details>` (ADR 0008):
/// the round number and how many findings remained open after it. Verdict is
/// implicit — zero means the round cleared everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRecord {
    pub round: u32,
    pub findings: usize,
}

/// What the review turn writes to [`REVIEW_FILE`].
#[derive(Debug, Deserialize)]
pub struct SelfReviewFile {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub review: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// What the fix turn writes to [`FIX_FILE`] (issue #212): one disposition per
/// open finding.
#[derive(Debug, Default, Deserialize)]
pub struct SelfReviewFixFile {
    #[serde(default)]
    pub dispositions: Vec<DispositionEntry>,
}

/// One entry in [`SelfReviewFixFile`]: the author's call on a single finding.
#[derive(Debug, Deserialize)]
pub struct DispositionEntry {
    pub id: String,
    pub action: Disposition,
    /// Required for `waived`; for a `decision` fixed, carries the decision.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The worker's self-review phase: review→fix until clean or the rounds cap,
/// then hand back to the flow to open the PR. Forge calls: zero on the happy
/// path. Interruption resumes from the checkpoint (rounds + ledger persist).
///
/// On a genuine escalation ([`NeedsHuman`] — a `needs_human` verdict, a real
/// ping-pong, or a failed review/fix turn) the escalate-time fallback runs
/// (issue #209, ADR 0021): if the branch is ahead of base, the committed work
/// is published as a needs-human draft PR before the error propagates. This is
/// the one place self-review touches the forge, and only when escalating —
/// `Stopped`/`Interrupted` (user stop, pane death) return `Ok(..)` and never
/// publish.
pub(crate) async fn self_review(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) -> Result<flow::StepFlow> {
    match self_review_inner(deps, run, cp, worktree, flavor).await {
        Err(e) if e.downcast_ref::<NeedsHuman>().is_some() => {
            // Best-effort: the draft is a bonus for the human, never a reason to
            // change the run's outcome — propagate the original NeedsHuman as-is.
            flow::publish_needs_human_draft(deps, run, cp, worktree, flavor).await;
            Err(e)
        }
        other => other,
    }
}

async fn self_review_inner(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) -> Result<flow::StepFlow> {
    let review_cfg = deps.config.review_for(&deps.project);
    let max_rounds = review_cfg.max_rounds;
    let lenses = review_cfg.lenses.clone();
    let kind = flavor.kind();
    let base = deps.project.default_branch.clone();
    let language = deps.config.language_for(&deps.project);

    // Resume-safety: the phase already finished — a clean convergence
    // (`self_review_converged`) or the cap→final-fix publish
    // (`self_review_final_fix_unreviewed`). Either way, don't re-review, and
    // don't let the cap backstop below mistake a done checkpoint for a
    // non-converged one (issue #176, #212).
    if cp.self_review_converged || cp.self_review_final_fix_unreviewed {
        return Ok(flow::StepFlow::Continue);
    }

    // Forward migration (issue #212): an in-flight run whose checkpoint predates
    // the ledger carries findings only in `self_review_pending`. Promote them so
    // the resumed run keeps its findings instead of re-reviewing from scratch.
    if cp.self_review_ledger.is_empty() && !cp.self_review_pending.is_empty() {
        promote_pending_to_ledger(cp);
        persist(deps, run, cp)?;
    }

    // Resume-safety (issue #212): the phase already committed to the cap→final-fix
    // path. Route straight back to it — don't fall into the loop's ping-pong
    // check, which the interrupted final fix's `fix_attempts` bump would trip.
    if cp.self_review_final_fix_started {
        return final_fix_and_publish(deps, run, cp, worktree, language).await;
    }

    loop {
        // Ping-pong is a property of the persisted ledger, so re-check it on
        // resume too: a crash right after a round persist must not downgrade a
        // ping-pong to a final-fix publish (issue #212, reason 2).
        if let Some((id, body, attempts)) = ping_pong(cp) {
            return escalate_ping_pong(deps, run, &id, &body, attempts);
        }

        // Backstop / resume guard: the cap is spent (only reachable via a prior
        // round's increment, so the round-max review already ran). Ping-pong and
        // needs_human/decision disputes escalate before this point, so what's
        // left is minor blocking — run the final fix and publish (ADR 0022).
        if cp.self_review_rounds >= max_rounds {
            return final_fix_and_publish(deps, run, cp, worktree, language).await;
        }

        // ---- review turn (in the self-review lane) ----
        let review = match review_turn(deps, run, cp, worktree, &base, kind, &lenses).await? {
            ReviewTurn::Reviewed(review) => review,
            ReviewTurn::Stopped => return Ok(flow::StepFlow::Stopped),
            ReviewTurn::Interrupted(r) => return Ok(flow::StepFlow::Interrupted(r)),
        };

        // `needs_human`: reasons 1 (reviewer verdict) and 3 (a disputed recorded
        // decision) both surface here — escalate at once, without spending a fix
        // round on something a fix cannot solve (ADR 0012/0022).
        if review.verdict == ReviewVerdict::NeedsHuman {
            deps.store.emit(
                Some(&run.id),
                EVENT_NEEDS_HUMAN,
                json!({ "round": cp.self_review_rounds + 1 }),
            )?;
            return Err(NeedsHuman(format!(
                "self-review flagged issue #{} for a human: {}",
                run.issue_number,
                review.review.trim()
            ))
            .into());
        }

        // Fold the review into the ledger, advance the incremental-diff cursor
        // (the HEAD this review looked at), and record the round.
        cp.self_review_rounds += 1;
        let round = cp.self_review_rounds;
        update_ledger_from_review(cp, &review, round);
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        cp.self_review_last_head = Some(head_now.trim().to_string());
        let open = open_count(cp);
        cp.self_review_log.push(RoundRecord {
            round,
            findings: open,
        });
        persist(deps, run, cp)?;
        deps.store.emit(
            Some(&run.id),
            "self_review.reviewed",
            json!({ "round": round, "verdict": review.verdict, "findings": open }),
        )?;

        // Reason 2: a real ping-pong — the round we just recorded left a finding
        // open after two fix turns.
        if let Some((id, body, attempts)) = ping_pong(cp) {
            return escalate_ping_pong(deps, run, &id, &body, attempts);
        }

        // Clean: no open finding left — converge and publish.
        if open == 0 {
            cp.self_review_converged = true;
            persist(deps, run, cp)?;
            deps.store
                .emit(Some(&run.id), EVENT_CLEAN, json!({ "rounds": round }))?;
            return Ok(flow::StepFlow::Continue);
        }

        // Cap reached with only minor blocking left → final fix + publish.
        if cp.self_review_rounds >= max_rounds {
            return final_fix_and_publish(deps, run, cp, worktree, language).await;
        }

        // ---- fix turn (in the author lane) ----
        match fix_turn(deps, run, cp, worktree, language).await? {
            flow::StepFlow::Continue => {}
            other => return Ok(other),
        }

        // Re-validate the fixed tree before the next review; a failing check
        // is fixed here (its own bounded corrective turns) so the review
        // always reads a green tree.
        match flow::validate(deps, run, cp, worktree, flow::STEP_SELF_REVIEW).await? {
            flow::StepFlow::Continue => {}
            other => return Ok(other),
        }
    }
}

/// Persist the checkpoint under the self-review step so a crash resumes here.
/// Mirrors the open ledger entries into `self_review_pending` first (issue #212
/// rollback safety valve): a binary rolled back past #212 reads only that field.
fn persist(deps: &Deps, run: &RunRecord, cp: &mut Checkpoint) -> Result<()> {
    mirror_open_to_pending(cp);
    deps.store
        .update_run_step(&run.id, flow::STEP_SELF_REVIEW, &serde_json::to_string(cp)?)?;
    Ok(())
}

/// The cap was reached with only minor blocking findings left (ping-pong and
/// decision disputes escalate earlier): run one final fix + validate, then
/// publish (issue #212, ADR 0022). The final fix is NOT re-reviewed, but the
/// check command and tree verification still gate it. Sets
/// `self_review_final_fix_unreviewed` only — never `self_review_converged` — so a
/// binary rolled back past #212 escalates the unreviewed fix instead of
/// publishing it as clean.
async fn final_fix_and_publish(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    language: Option<&str>,
) -> Result<flow::StepFlow> {
    // Degenerate (e.g. cap reached with an empty ledger): nothing to fix — clean.
    if open_count(cp) == 0 {
        cp.self_review_converged = true;
        persist(deps, run, cp)?;
        deps.store.emit(
            Some(&run.id),
            EVENT_CLEAN,
            json!({ "rounds": cp.self_review_rounds }),
        )?;
        return Ok(flow::StepFlow::Continue);
    }

    // Commit to this path atomically, before the fix turn bumps `fix_attempts`:
    // a resume then routes back here (phase entry) instead of mis-reading the
    // bump as a ping-pong (issue #212).
    cp.self_review_final_fix_started = true;
    persist(deps, run, cp)?;

    match fix_turn(deps, run, cp, worktree, language).await? {
        flow::StepFlow::Continue => {}
        other => return Ok(other),
    }
    match flow::validate(deps, run, cp, worktree, flow::STEP_SELF_REVIEW).await? {
        flow::StepFlow::Continue => {}
        other => return Ok(other),
    }

    cp.self_review_final_fix_unreviewed = true;
    persist(deps, run, cp)?;
    deps.store.emit(
        Some(&run.id),
        EVENT_FINAL_FIX,
        json!({ "rounds": cp.self_review_rounds, "pending": open_count(cp) }),
    )?;
    Ok(flow::StepFlow::Continue)
}

/// Emit the ping-pong event and fail the run to a human (issue #212, reason 2).
fn escalate_ping_pong(
    deps: &Deps,
    run: &RunRecord,
    id: &str,
    body: &str,
    attempts: u32,
) -> Result<flow::StepFlow> {
    deps.store.emit(
        Some(&run.id),
        EVENT_PINGPONG,
        json!({ "id": id, "fix_attempts": attempts }),
    )?;
    Err(NeedsHuman(format!(
        "self-review ping-pong on issue #{}: finding `{id}` ({body}) is still open after \
         {attempts} fix turns",
        run.issue_number,
    ))
    .into())
}

/// Count the open ledger entries — convergence is "this is zero".
fn open_count(cp: &Checkpoint) -> usize {
    cp.self_review_ledger
        .iter()
        .filter(|e| e.status == FindingStatus::Open)
        .count()
}

/// A real ping-pong (issue #212, reason 2): a finding still open after two fix
/// turns. Returns `(id, body, fix_attempts)` for the escalation message.
fn ping_pong(cp: &Checkpoint) -> Option<(String, String, u32)> {
    cp.self_review_ledger
        .iter()
        .find(|e| e.status == FindingStatus::Open && e.fix_attempts >= 2)
        .map(|e| (e.id.clone(), e.body.trim().to_string(), e.fix_attempts))
}

/// Promote a pre-#212 `self_review_pending` snapshot into the ledger on resume:
/// each pending finding becomes an open entry (id assigned if it lacks one).
fn promote_pending_to_ledger(cp: &mut Checkpoint) {
    let round = cp.self_review_rounds;
    let pending = cp.self_review_pending.clone();
    for (i, f) in pending.into_iter().enumerate() {
        let counter = i + 1;
        cp.self_review_ledger.push(LedgerEntry {
            id: f.id.clone().unwrap_or_else(|| format!("f{counter}")),
            kind: f.kind,
            path: f.path,
            line: f.line,
            body: f.body,
            lens: f.lens,
            status: FindingStatus::Open,
            author_disposition: None,
            fix_attempts: 0,
            waive_reason: None,
            origin_round: round,
        });
    }
}

/// Mirror the open ledger entries into `self_review_pending` (issue #212
/// rollback safety valve). The `Finding` shape carries the id so a re-listed
/// finding stays identifiable.
fn mirror_open_to_pending(cp: &mut Checkpoint) {
    cp.self_review_pending = cp
        .self_review_ledger
        .iter()
        .filter(|e| e.status == FindingStatus::Open)
        .map(|e| Finding {
            id: Some(e.id.clone()),
            kind: e.kind,
            path: e.path.clone(),
            line: e.line,
            body: e.body.clone(),
            lens: e.lens.clone(),
        })
        .collect();
}

/// Fold a review turn into the ledger (issue #212): omitted open findings are
/// resolved (the author's disposition decides fixed vs waived), re-listed ones
/// stay/return to open, and unmatched findings are appended as new open entries.
/// Only a review turn moves status — never the author.
fn update_ledger_from_review(cp: &mut Checkpoint, review: &SelfReviewFile, round: u32) {
    let existing: HashSet<String> = cp.self_review_ledger.iter().map(|e| e.id.clone()).collect();
    let relisted: HashSet<String> = review
        .findings
        .iter()
        .filter_map(|f| f.id.clone())
        .filter(|id| existing.contains(id))
        .collect();

    // Resolve every open entry the reviewer did NOT re-list; the author's last
    // disposition decides whether it lands fixed or waived.
    for e in cp.self_review_ledger.iter_mut() {
        if e.status == FindingStatus::Open && !relisted.contains(&e.id) {
            e.status = match e.author_disposition {
                Some(Disposition::Waived) => FindingStatus::Waived,
                _ => FindingStatus::Fixed,
            };
        }
    }
    // Re-open any re-listed entry that had been resolved (reviewer changed mind).
    for e in cp.self_review_ledger.iter_mut() {
        if relisted.contains(&e.id) && e.status != FindingStatus::Open {
            e.status = FindingStatus::Open;
        }
    }
    // Append new findings (no id, or an id that matches nothing existing).
    let mut counter = cp
        .self_review_ledger
        .iter()
        .filter_map(|e| e.id.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    for f in &review.findings {
        let is_existing = f.id.as_ref().is_some_and(|id| existing.contains(id));
        if is_existing {
            continue;
        }
        counter += 1;
        cp.self_review_ledger.push(LedgerEntry {
            id: format!("f{counter}"),
            kind: f.kind,
            path: f.path.clone(),
            line: f.line,
            body: f.body.clone(),
            lens: f.lens.clone(),
            status: FindingStatus::Open,
            author_disposition: None,
            fix_attempts: 0,
            waive_reason: None,
            origin_round: round,
        });
    }
}

/// Apply a fix turn's dispositions to the ledger (issue #212): record the
/// author's claim and bump the fix-attempt counter for each open finding. Status
/// stays open — only the next review resolves it.
fn apply_dispositions(cp: &mut Checkpoint, fix: &SelfReviewFixFile) {
    for d in &fix.dispositions {
        if let Some(e) = cp
            .self_review_ledger
            .iter_mut()
            .find(|e| e.id == d.id && e.status == FindingStatus::Open)
        {
            e.author_disposition = Some(d.action);
            e.fix_attempts += 1;
            match d.action {
                Disposition::Waived => e.waive_reason = d.reason.clone(),
                // A `decision` fixed carries the decision content in `reason`.
                Disposition::Fixed => {
                    if e.kind == FindingKind::Decision && d.reason.is_some() {
                        e.waive_reason = d.reason.clone();
                    }
                }
            }
        }
    }
}

/// Validate a fix file against the open ledger (issue #212): every open finding
/// needs a disposition, and a waive needs a reason. The Err text feeds a
/// corrective prompt.
fn validate_fix_file(cp: &Checkpoint, fix: &SelfReviewFixFile) -> std::result::Result<(), String> {
    let provided: HashMap<&str, &DispositionEntry> = fix
        .dispositions
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();
    for e in cp
        .self_review_ledger
        .iter()
        .filter(|e| e.status == FindingStatus::Open)
    {
        match provided.get(e.id.as_str()) {
            None => {
                return Err(format!(
                    "- finding `{}` has no disposition in `{FIX_FILE}`; every open finding needs \
                     an entry with `action` \"fixed\" or \"waived\"",
                    e.id
                ));
            }
            Some(d)
                if d.action == Disposition::Waived
                    && d.reason.as_deref().map(str::trim).unwrap_or("").is_empty() =>
            {
                return Err(format!(
                    "- finding `{}` is waived but has no `reason`; a waive must say why you disagree",
                    e.id
                ));
            }
            // A `decision` marked `fixed` must record the chosen option in
            // `reason` — the ledger keeps the decision, and the next review
            // confirms it was recorded (issue #212, ADR 0022).
            Some(d)
                if e.kind == FindingKind::Decision
                    && d.action == Disposition::Fixed
                    && d.reason.as_deref().map(str::trim).unwrap_or("").is_empty() =>
            {
                return Err(format!(
                    "- decision finding `{}` is marked fixed but has no `reason`; record the \
                     option you chose in `reason` so it lands in the ledger",
                    e.id
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

enum ReviewTurn {
    Reviewed(SelfReviewFile),
    Stopped,
    Interrupted(String),
}

/// One review turn (plus at most one corrective turn). The review runs in the
/// `self-review` lane; verification is the orchestrator's: the checkout must
/// stay pristine and at the same HEAD, and the review file must parse. Round 2+
/// also drops an incremental diff since the previous review (issue #212).
async fn review_turn(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
    worktree: &Path,
    base: &str,
    kind: Kind,
    lenses: &[String],
) -> Result<ReviewTurn> {
    // Drop the local base diff where the prompt says it is, and clear any stale
    // review file so we read *this* turn's verdict.
    let base_diff = gitops::diff_against_base(worktree, base).await?;
    std::fs::create_dir_all(worktree.join(MEGURI_DIR))?;
    std::fs::write(worktree.join(DIFF_FILE), &base_diff)?;
    let _ = std::fs::remove_file(worktree.join(REVIEW_FILE));

    // Round 2+: also drop the incremental diff since the last review's HEAD, so
    // the reviewer sees exactly what the fix turns changed (issue #212).
    let round = cp.self_review_rounds + 1;
    let has_incremental = if round > 1 {
        if let Some(last) = cp.self_review_last_head.as_deref() {
            let inc = gitops::diff_between(worktree, last, "HEAD").await?;
            std::fs::write(worktree.join(INCREMENTAL_DIFF_FILE), &inc)?;
            true
        } else {
            let _ = std::fs::remove_file(worktree.join(INCREMENTAL_DIFF_FILE));
            false
        }
    } else {
        let _ = std::fs::remove_file(worktree.join(INCREMENTAL_DIFF_FILE));
        false
    };

    let language = deps.config.language_for(&deps.project);
    let head_before = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
    let mut prompt = review_prompt(run, cp, kind, lenses, language, has_incremental);
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) =
            flow::run_review_turn(deps, run, worktree, "self-review", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(ReviewTurn::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(ReviewTurn::Interrupted(
                    "pane died during self-review".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure => {
                return Err(NeedsHuman(format!(
                    "agent reported failure self-reviewing issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            // needs_plan/decompose make no sense on a review turn once work is
            // committed — a human looks.
            TurnStatus::NeedsHuman | TurnStatus::NeedsPlan | TurnStatus::Decompose => {
                return Err(NeedsHuman(format!(
                    "agent needs a human self-reviewing issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }

        // Trust but verify: the review turn must not touch the tree.
        let clean = gitops::status_clean(worktree).await?;
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        let problem = if !clean || head_now != head_before {
            Some(format!(
                "- the review must not modify the tree: working tree clean: \
                 {clean} (must be true), HEAD: {head_now} (must be {head_before}) — \
                 discard any changes; write only your review to `{REVIEW_FILE}`"
            ))
        } else {
            read_review(worktree).err()
        };
        let Some(problem) = problem else {
            return Ok(ReviewTurn::Reviewed(
                read_review(worktree).expect("verified above"),
            ));
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent claimed a self-review but it doesn't verify after a \
                 corrective turn:\n{problem}"
            ))
            .into());
        }
        deps.store.emit(
            Some(&run.id),
            EVENT_CORRECTION,
            json!({ "problem": problem }),
        )?;
        prompt = format!(
            "Your previous result claimed a completed review, but verification failed:\n{problem}\n\n\
             Fix this. Do not modify the checkout; write your review to `{REVIEW_FILE}` as instructed.",
        );
    }
}

/// One fix turn (plus at most one corrective turn) in the author lane: the
/// author addresses the open findings, declares a disposition per finding in
/// [`FIX_FILE`], and commits, leaving a clean tree. Applies the dispositions to
/// the ledger on success (issue #212).
async fn fix_turn(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    language: Option<&str>,
) -> Result<flow::StepFlow> {
    let _ = std::fs::remove_file(worktree.join(FIX_FILE));
    let open: Vec<LedgerEntry> = cp
        .self_review_ledger
        .iter()
        .filter(|e| e.status == FindingStatus::Open)
        .cloned()
        .collect();
    let mut prompt = fix_prompt(&open, language);
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = flow::run_turn(deps, run, worktree, "self-review-fix", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(flow::StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(flow::StepFlow::Interrupted(
                    "pane died during self-review fix".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure | TurnStatus::NeedsHuman => {
                return Err(NeedsHuman(format!(
                    "agent could not fix its self-review findings on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            // needs_plan/decompose are meaningless once the work is committed
            // and merely being polished — a human looks.
            TurnStatus::NeedsPlan | TurnStatus::Decompose => {
                return Err(NeedsHuman(format!(
                    "agent asked to re-plan while fixing its self-review on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }

        // Two invariants: the tree is clean (nothing uncommitted to push) and
        // the disposition file accounts for every open finding.
        let problem = if !gitops::status_clean(worktree).await? {
            Some(
                "- your working tree is not clean; commit (or discard) every change so nothing \
                 dangles"
                    .to_string(),
            )
        } else {
            match read_fix_file(worktree) {
                Err(e) => Some(e),
                Ok(fix) => validate_fix_file(cp, &fix).err(),
            }
        };

        let Some(problem) = problem else {
            let fix = read_fix_file(worktree).expect("validated above");
            apply_dispositions(cp, &fix);
            persist(deps, run, cp)?;
            deps.store.emit(
                Some(&run.id),
                "self_review.fixed",
                json!({ "round": cp.self_review_rounds }),
            )?;
            return Ok(flow::StepFlow::Continue);
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent's self-review fix on issue #{} doesn't verify after a corrective turn:\n{problem}",
                run.issue_number
            ))
            .into());
        }
        deps.store.emit(
            Some(&run.id),
            "self_review.fix_correction",
            json!({ "round": cp.self_review_rounds, "problem": problem }),
        )?;
        prompt = format!(
            "Your previous fix turn did not verify:\n{problem}\n\n\
             Fix this: commit all code changes (leave a clean tree) and write `{FIX_FILE}` with a \
             disposition for every finding. Do not create a pull request; meguri handles that."
        );
    }
}

/// The multi-lens review instruction (ADR 0008): one review turn considers
/// every configured perspective. For a spec/ADR (Plan) the code lenses are
/// re-read as document lenses; for code (Impl) they are taken literally.
fn lens_instruction(kind: Kind, lenses: &[String]) -> String {
    if lenses.is_empty() {
        return String::new();
    }
    let list = lenses
        .iter()
        .map(|l| format!("`{l}`"))
        .collect::<Vec<_>>()
        .join(", ");
    match kind {
        Kind::Plan => format!(
            "- Review through each of these lenses, adapted to a design document: {list} \
             (e.g. `correctness` = are the decisions sound and internally consistent; \
             `tests` = is the plan verifiable / are acceptance criteria present; \
             `simplicity` = is the scope minimal; `security` = are risks acknowledged).\n"
        ),
        Kind::Impl => format!("- Review through each of these lenses: {list}.\n"),
    }
}

/// The JSON schema the review turn writes (shared by round 1 and round 2+).
fn review_schema() -> &'static str {
    "`{\"verdict\": \"clean\" | \"fixable\" | \"needs_human\", \"review\": \"<Markdown summary>\", \
     \"findings\": [{\"id\": null, \"kind\": \"defect\" | \"decision\", \"path\": \"src/x.rs\", \
     \"line\": 42, \"lens\": \"correctness\", \"body\": \"<what must change>\"}]}`"
}

fn review_prompt(
    run: &RunRecord,
    cp: &Checkpoint,
    kind: Kind,
    lenses: &[String],
    language: Option<&str>,
    has_incremental: bool,
) -> String {
    let round = cp.self_review_rounds + 1;
    let subject = match kind {
        Kind::Plan => "spec/ADR",
        Kind::Impl => "implementation",
    };
    if round == 1 {
        return format!(
            "You are self-reviewing your own {subject} of issue #{number} before it is \
             published as a pull request (self-review round 1). The worktree holds the \
             committed work; `{diff}` is its full diff against the base branch.\n\n\
             # Issue: {title}\n\n\
             # Instructions\n\
             - Read the diff at `{diff}`; browse the checked-out files for context as needed.\n\
             {lens_section}\
             - Review the {subject} for correctness, completeness (tests included where the \
               change is code), and fit with the repository's conventions.\n\
             - Do NOT modify, commit, or push anything; the review file below is your only \
               deliverable.\n\
             - Write your review to `{review}` as JSON:\n  {schema}\n\
               - \"clean\": nothing must change before this can be published (pure nitpicks do not \
                 block; mention them in `review` and leave `findings` empty).\n\
               - \"fixable\": something must change and you can fix it. Every finding is blocking — \
                 there is no severity, so keep non-blocking remarks in `review` prose only. Anchor \
                 each finding to a line on the NEW side of the diff, leave `id` null (it is assigned \
                 for you), and set `kind`: \"defect\" for a bug/omission you fix in code, \
                 \"decision\" for an A-or-B you must settle and record in the {subject}.\n\
               - \"needs_human\": something needs a person to decide — an ambiguous requirement, a \
                 risky trade-off, a product/design call you cannot make from the code. Explain it in \
                 `review` and leave `findings` empty; this stops the run.\n\
             - A completed review is a success regardless of verdict; report \"failure\"/\"needs_human\" \
               as the turn status only when you cannot review at all (the verdict above is the \
               review's conclusion, not the turn's).\
             {lang_section}",
            number = run.issue_number,
            title = cp.issue_title,
            diff = DIFF_FILE,
            review = REVIEW_FILE,
            schema = review_schema(),
            lens_section = lens_instruction(kind, lenses),
            lang_section = flow::language_instruction(language),
        );
    }

    let incremental_line = if has_incremental {
        format!(
            "- `{INCREMENTAL_DIFF_FILE}` — the incremental diff since your last review \
             (what the fix turns changed).\n"
        )
    } else {
        String::new()
    };
    format!(
        "You are self-reviewing your own {subject} of issue #{number} before publication \
         (self-review round {round}). This is NOT a fresh review: your job is to CONFIRM whether \
         your earlier findings are resolved and to add ONLY new blocking issues.\n\n\
         # Issue: {title}\n\n\
         # Prior findings (the ledger)\n{ledger}\n\
         # Diffs\n\
         - `{diff}` — the full diff against the base branch.\n\
         {incremental_line}\n\
         # Instructions\n\
         {lens_section}\
         - For each prior finding above: if it is now resolved, OMIT it. If it is still not \
           addressed, RE-LIST it by repeating its exact `id`. Do not re-argue findings you already \
           resolved, and do not re-review parts of the diff unrelated to an open finding.\n\
         - A `decision` finding is resolved once the decision is recorded in the {subject}; only \
           check that it was recorded — do NOT re-litigate which option was chosen. If you believe a \
           recorded decision is wrong and a human must overrule it, use \"needs_human\".\n\
         - You MAY add new findings, but only genuinely blocking ones (every finding is blocking; no \
           severity — keep lesser remarks in `review` prose). Leave `id` null on new findings.\n\
         - Do NOT modify, commit, or push anything; the review file below is your only deliverable.\n\
         - Write your review to `{review}` as JSON:\n  {schema}\n\
           - \"clean\": every prior finding is resolved and you add no new blocking finding (leave \
             `findings` empty).\n\
           - \"fixable\": at least one finding remains or is newly added — list them (re-listed ones \
             keep their `id`, new ones leave `id` null).\n\
           - \"needs_human\": a person must decide (e.g. a disputed recorded decision); explain in \
             `review` and leave `findings` empty.\
         {lang_section}",
        number = run.issue_number,
        title = cp.issue_title,
        ledger = render_ledger_for_prompt(cp),
        diff = DIFF_FILE,
        review = REVIEW_FILE,
        schema = review_schema(),
        lens_section = lens_instruction(kind, lenses),
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

fn kind_str(kind: FindingKind) -> &'static str {
    match kind {
        FindingKind::Defect => "defect",
        FindingKind::Decision => "decision",
    }
}

/// Render the open ledger entries for the round 2+ review prompt: id, kind,
/// anchor, body, the author's last disposition, and any waive reason / recorded
/// decision for context.
fn render_ledger_for_prompt(cp: &Checkpoint) -> String {
    let mut out = String::new();
    for e in cp
        .self_review_ledger
        .iter()
        .filter(|e| e.status == FindingStatus::Open)
    {
        let disp = match e.author_disposition {
            Some(Disposition::Fixed) => "author: addressed",
            Some(Disposition::Waived) => "author: disagrees (waived)",
            None => "not yet addressed",
        };
        out.push_str(&format!(
            "- `{}` [{}] {}:{} — {} ({}",
            e.id,
            kind_str(e.kind),
            e.path,
            e.line,
            e.body,
            disp
        ));
        if let Some(r) = &e.waive_reason {
            out.push_str(&format!("; reason/decision: {r}"));
        }
        out.push_str(")\n");
    }
    if out.is_empty() {
        out.push_str("(no open findings)\n");
    }
    out
}

fn fix_prompt(open: &[LedgerEntry], language: Option<&str>) -> String {
    let list = if open.is_empty() {
        "(no line-anchored findings — see the review summary from your last turn)".to_string()
    } else {
        open.iter()
            .map(|e| {
                let hint = match e.kind {
                    FindingKind::Decision => {
                        " (decision — settle A-or-B and record it in the spec)"
                    }
                    FindingKind::Defect => "",
                };
                let prior = match (e.author_disposition, e.waive_reason.as_deref()) {
                    (Some(Disposition::Waived), Some(r)) => {
                        format!(" [you waived this before: {r}; the reviewer re-raised it]")
                    }
                    _ => String::new(),
                };
                format!(
                    "- `{}` [{}] `{}:{}` — {}{hint}{prior}",
                    e.id,
                    kind_str(e.kind),
                    e.path,
                    e.line,
                    e.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Your self-review found issues in your own diff. Address each finding, then commit \
         your fixes and record your disposition.\n\n\
         # Findings\n{list}\n\n\
         # Instructions\n\
         - Fix each `defect` you agree with. For a `decision`, choose an option and record the \
           choice in the spec/impl (do not leave it open).\n\
         - If you genuinely disagree with a finding, you may waive it instead of changing code — but \
           give a reason. The next review round decides whether to accept the waive or re-raise it.\n\
         - Run the relevant tests/checks yourself.\n\
         - Write `{fix}` as JSON declaring, for EVERY finding above, what you did:\n  \
           `{{\"dispositions\": [{{\"id\": \"f1\", \"action\": \"fixed\"}}, \
           {{\"id\": \"f2\", \"action\": \"waived\", \"reason\": \"why you disagree\"}}]}}`\n  \
           - `fixed`: you addressed it in code (for a `decision`, put the chosen option in `reason`).\n  \
           - `waived`: you disagree; `reason` is required.\n\
         - COMMIT all your code changes to the current branch with clear messages. Leave the working \
           tree clean (the `{fix}` file lives under `.meguri/`, which is git-excluded).\n\
         - Do NOT push and do NOT create a pull request; meguri handles both.\
         {lang_section}",
        list = list,
        fix = FIX_FILE,
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

/// Parse and validate the review file. The Err text feeds a corrective prompt.
fn read_review(worktree: &Path) -> std::result::Result<SelfReviewFile, String> {
    let raw = std::fs::read_to_string(worktree.join(REVIEW_FILE)).map_err(|_| {
        format!("- review file `{REVIEW_FILE}` does not exist (write it as instructed)")
    })?;
    let review: SelfReviewFile = serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- review file `{REVIEW_FILE}` is not valid JSON ({e}); expected \
             {{\"verdict\": \"clean\" | \"fixable\" | \"needs_human\", \"review\": \"<Markdown>\", \
             \"findings\": [{{\"id\": null, \"kind\": \"defect\"|\"decision\", \"path\": ..., \
             \"line\": ..., \"body\": ...}}]}}"
        )
    })?;
    if review.verdict != ReviewVerdict::Clean && review.review.trim().is_empty() {
        return Err(format!(
            "- verdict is \"{:?}\" but `review` in `{REVIEW_FILE}` is empty; \
             a non-clean verdict must explain what must change",
            review.verdict
        ));
    }
    // `fixable ⇔ findings non-empty`, enforced both directions (issue #212).
    match review.verdict {
        ReviewVerdict::Fixable if review.findings.is_empty() => {
            return Err(format!(
                "- verdict is \"fixable\" but `findings` in `{REVIEW_FILE}` is empty; a fixable \
                 review must carry at least one line-anchored finding (use \"clean\" if nothing \
                 blocks, or \"needs_human\" if a person must decide)"
            ));
        }
        ReviewVerdict::Clean if !review.findings.is_empty() => {
            return Err(format!(
                "- verdict is \"clean\" but `findings` in `{REVIEW_FILE}` is not empty; \
                 a clean review carries no findings — move the remarks into `review` \
                 or change the verdict"
            ));
        }
        ReviewVerdict::NeedsHuman if !review.findings.is_empty() => {
            return Err(format!(
                "- verdict is \"needs_human\" but `findings` in `{REVIEW_FILE}` is not empty; \
                 a needs_human review carries no findings — explain the decision in `review`"
            ));
        }
        _ => {}
    }
    for f in &review.findings {
        if f.path.trim().is_empty() || f.line == 0 || f.body.trim().is_empty() {
            return Err(format!(
                "- every `findings` entry in `{REVIEW_FILE}` needs a non-empty \
                 `path`, a `line` >= 1 on the NEW side of the diff, and a \
                 non-empty `body`"
            ));
        }
    }
    Ok(review)
}

/// Parse the fix file. The Err text feeds a corrective prompt.
fn read_fix_file(worktree: &Path) -> std::result::Result<SelfReviewFixFile, String> {
    let raw = std::fs::read_to_string(worktree.join(FIX_FILE)).map_err(|_| {
        format!("- fix file `{FIX_FILE}` does not exist (write your dispositions there)")
    })?;
    serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- fix file `{FIX_FILE}` is not valid JSON ({e}); expected \
             {{\"dispositions\": [{{\"id\": \"f1\", \"action\": \"fixed\" | \"waived\", \
             \"reason\": \"...\"}}]}}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_run() -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", "worker", 7, "Add caching")
            .unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.issue_title = Some("Add caching".into());
        run
    }

    fn fake_deps() -> Deps {
        use std::sync::Arc;
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            check_command: None,
            worktree_root: None,
            language: None,
            pr: None,
            clean: None,
            triage: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            autonomy: None,
            cadence: Vec::new(),
            prompts: Default::default(),
            notify: None,
        };
        Deps::with_label_source(
            crate::store::Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(crate::forge::fake::FakeForge::default()),
            crate::config::Config::default(),
            project,
        )
    }

    /// A stand-in flavor whose only reachable method on the converged path is
    /// `kind()` (defaulted); everything else must never be called.
    struct DoneFlavor;

    #[async_trait::async_trait]
    impl Flavor for DoneFlavor {
        fn trigger_label(&self) -> &'static str {
            ""
        }
        fn execute_prompt(&self, _: &Deps, _: &RunRecord, _: &Checkpoint, _: &Path) -> String {
            unreachable!("not reached on the converged path")
        }
        fn verify_work(
            &self,
            _: &RunRecord,
            _: &Checkpoint,
            _: &Path,
        ) -> std::result::Result<(), String> {
            unreachable!("not reached on the converged path")
        }
        fn pr_title(&self, _: &RunRecord, _: &Checkpoint) -> String {
            unreachable!("not reached on the converged path")
        }
        async fn settle_labels(&self, _: &Deps, _: &RunRecord, _: &Checkpoint) -> Result<()> {
            unreachable!("not reached on the converged path")
        }
    }

    /// Resume-safety (issue #176): a clean verdict on the last allowed round
    /// persists `self_review_converged` with `rounds == max_rounds`. On resume
    /// the phase must publish (Continue), not re-run the cap backstop.
    #[tokio::test]
    async fn converged_checkpoint_short_circuits_without_re_review() {
        let deps = fake_deps();
        let run = fake_run();
        let mut cp = Checkpoint {
            self_review_converged: true,
            self_review_rounds: deps.config.review.max_rounds,
            ..Default::default()
        };
        let flow = self_review(&deps, &run, &mut cp, Path::new("/nonexistent"), &DoneFlavor)
            .await
            .unwrap();
        assert!(matches!(flow, flow::StepFlow::Continue));
    }

    /// Resume-safety (issue #212): the cap→final-fix publish sets only
    /// `self_review_final_fix_unreviewed` (never `converged`). On resume the
    /// phase must still short-circuit to Continue, not re-review.
    #[tokio::test]
    async fn final_fix_checkpoint_short_circuits_without_re_review() {
        let deps = fake_deps();
        let run = fake_run();
        let mut cp = Checkpoint {
            self_review_final_fix_unreviewed: true,
            self_review_rounds: deps.config.review.max_rounds,
            ..Default::default()
        };
        let flow = self_review(&deps, &run, &mut cp, Path::new("/nonexistent"), &DoneFlavor)
            .await
            .unwrap();
        assert!(matches!(flow, flow::StepFlow::Continue));
    }

    fn cp_with_title() -> Checkpoint {
        Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        }
    }

    fn entry(id: &str, kind: FindingKind, status: FindingStatus) -> LedgerEntry {
        LedgerEntry {
            id: id.into(),
            kind,
            path: "src/a.rs".into(),
            line: 1,
            body: "x".into(),
            lens: None,
            status,
            author_disposition: None,
            fix_attempts: 0,
            waive_reason: None,
            origin_round: 1,
        }
    }

    fn finding(id: Option<&str>, kind: FindingKind) -> Finding {
        Finding {
            id: id.map(str::to_string),
            kind,
            path: "src/a.rs".into(),
            line: 1,
            body: "x".into(),
            lens: None,
        }
    }

    #[test]
    fn review_file_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".meguri")).unwrap();
        let path = dir.path().join(REVIEW_FILE);

        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");

        std::fs::write(&path, "not json").unwrap();
        assert!(
            read_review(dir.path())
                .unwrap_err()
                .contains("not valid JSON")
        );

        std::fs::write(&path, r#"{"verdict":"findings","review":"  "}"#).unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("empty"));

        // Clean must carry no findings.
        std::fs::write(
            &path,
            r#"{"verdict":"clean","review":"ok","findings":[{"path":"a.rs","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("clean"));

        // `fixable` with no findings is rejected (both-directions, issue #212).
        std::fs::write(&path, r#"{"verdict":"fixable","review":"r","findings":[]}"#).unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("fixable"));

        // `needs_human` must carry no findings (issue #212).
        std::fs::write(
            &path,
            r#"{"verdict":"needs_human","review":"r","findings":[{"path":"a.rs","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("needs_human"));

        // Findings must be fully anchored.
        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"r","findings":[{"path":"","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("non-empty"));

        // The legacy "findings" verdict still parses as the `fixable` alias, and
        // `kind` defaults to defect when omitted.
        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"- bug","findings":[{"path":"src/a.rs","line":42,"body":"off by one"}]}"#,
        )
        .unwrap();
        let review = read_review(dir.path()).unwrap();
        assert_eq!(review.verdict, ReviewVerdict::Fixable);
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.findings[0].line, 42);
        assert_eq!(review.findings[0].kind, FindingKind::Defect);
        assert!(review.findings[0].id.is_none());

        // A decision finding with an id round-trips.
        std::fs::write(
            &path,
            r#"{"verdict":"fixable","review":"- pick","findings":[{"id":"f3","kind":"decision","path":"src/a.rs","line":7,"body":"A or B"}]}"#,
        )
        .unwrap();
        let review = read_review(dir.path()).unwrap();
        assert_eq!(review.findings[0].kind, FindingKind::Decision);
        assert_eq!(review.findings[0].id.as_deref(), Some("f3"));

        std::fs::write(&path, r#"{"verdict":"clean"}"#).unwrap();
        assert_eq!(
            read_review(dir.path()).unwrap().verdict,
            ReviewVerdict::Clean
        );
    }

    #[test]
    fn round1_review_prompt_demands_kinded_anchored_findings() {
        let run = fake_run();
        let prompt = review_prompt(
            &run,
            &cp_with_title(),
            Kind::Impl,
            &["correctness".to_string(), "security".to_string()],
            None,
            false,
        );
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains(DIFF_FILE));
        assert!(prompt.contains(REVIEW_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("NEW side of the diff"));
        assert!(prompt.contains("self-review round 1"));
        assert!(prompt.contains("\"defect\""));
        assert!(prompt.contains("\"decision\""));
        assert!(prompt.contains("`correctness`"));
        assert!(prompt.contains("`security`"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn round2_review_prompt_carries_ledger_and_increment() {
        let run = fake_run();
        let mut cp = cp_with_title();
        cp.self_review_rounds = 1;
        cp.self_review_ledger = vec![
            LedgerEntry {
                author_disposition: Some(Disposition::Waived),
                waive_reason: Some("dup of X".into()),
                ..entry("f1", FindingKind::Defect, FindingStatus::Open)
            },
            entry("f2", FindingKind::Decision, FindingStatus::Fixed),
        ];
        let prompt = review_prompt(&run, &cp, Kind::Impl, &["correctness".into()], None, true);
        assert!(prompt.contains("self-review round 2"));
        // The open ledger entry (and its waive reason) is shown; the resolved
        // one is not re-surfaced as an action item.
        assert!(prompt.contains("`f1`"));
        assert!(prompt.contains("dup of X"));
        assert!(prompt.contains(INCREMENTAL_DIFF_FILE));
        assert!(prompt.contains("RE-LIST"));
        assert!(prompt.contains("do NOT re-litigate"));
    }

    #[test]
    fn fix_prompt_lists_findings_and_demands_dispositions() {
        let open = vec![
            LedgerEntry {
                body: "handle the None case".into(),
                ..entry("f1", FindingKind::Defect, FindingStatus::Open)
            },
            LedgerEntry {
                body: "pick a storage backend".into(),
                ..entry("f2", FindingKind::Decision, FindingStatus::Open)
            },
        ];
        let prompt = fix_prompt(&open, Some("日本語"));
        assert!(prompt.contains("`f1`"));
        assert!(prompt.contains("handle the None case"));
        assert!(prompt.contains("decision — settle"));
        assert!(prompt.contains(FIX_FILE));
        assert!(prompt.contains("waived"));
        assert!(prompt.contains("Do NOT push"));
        assert!(prompt.contains("# Output language"));

        let prompt = fix_prompt(&[], None);
        assert!(prompt.contains("no line-anchored findings"));
    }

    #[test]
    fn ledger_resolves_omitted_and_keeps_relisted() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![
            LedgerEntry {
                author_disposition: Some(Disposition::Fixed),
                ..entry("f1", FindingKind::Defect, FindingStatus::Open)
            },
            LedgerEntry {
                author_disposition: Some(Disposition::Waived),
                waive_reason: Some("dup".into()),
                ..entry("f2", FindingKind::Defect, FindingStatus::Open)
            },
        ];
        // Round 2: reviewer re-lists f1 (rejects the fix), omits f2 (accepts the
        // waive), and adds a new finding.
        let review = SelfReviewFile {
            verdict: ReviewVerdict::Fixable,
            review: "r".into(),
            findings: vec![
                finding(Some("f1"), FindingKind::Defect),
                finding(None, FindingKind::Defect),
            ],
        };
        update_ledger_from_review(&mut cp, &review, 2);
        let f1 = cp.self_review_ledger.iter().find(|e| e.id == "f1").unwrap();
        let f2 = cp.self_review_ledger.iter().find(|e| e.id == "f2").unwrap();
        assert_eq!(f1.status, FindingStatus::Open, "re-listed stays open");
        assert_eq!(f2.status, FindingStatus::Waived, "omitted waive accepted");
        // The new finding got a fresh id past the highest existing one.
        let f3 = cp.self_review_ledger.iter().find(|e| e.id == "f3").unwrap();
        assert_eq!(f3.status, FindingStatus::Open);
        assert_eq!(f3.origin_round, 2);
    }

    #[test]
    fn omitted_fixed_finding_resolves() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![LedgerEntry {
            author_disposition: Some(Disposition::Fixed),
            ..entry("f1", FindingKind::Defect, FindingStatus::Open)
        }];
        let review = SelfReviewFile {
            verdict: ReviewVerdict::Clean,
            review: String::new(),
            findings: vec![],
        };
        update_ledger_from_review(&mut cp, &review, 2);
        assert_eq!(cp.self_review_ledger[0].status, FindingStatus::Fixed);
        assert_eq!(open_count(&cp), 0);
    }

    /// The status transition (finding #2 of the spec review): the author's
    /// disposition never closes a finding; two rejected fix turns are a
    /// ping-pong.
    #[test]
    fn two_rejected_fix_turns_are_a_ping_pong() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![entry("f1", FindingKind::Defect, FindingStatus::Open)];

        // Fix turn 1: author claims fixed. Status stays open, attempts = 1.
        apply_dispositions(
            &mut cp,
            &SelfReviewFixFile {
                dispositions: vec![DispositionEntry {
                    id: "f1".into(),
                    action: Disposition::Fixed,
                    reason: None,
                }],
            },
        );
        assert_eq!(cp.self_review_ledger[0].status, FindingStatus::Open);
        assert_eq!(cp.self_review_ledger[0].fix_attempts, 1);
        assert!(ping_pong(&cp).is_none());

        // Review re-lists f1 (rejects), then fix turn 2 claims fixed again.
        update_ledger_from_review(
            &mut cp,
            &SelfReviewFile {
                verdict: ReviewVerdict::Fixable,
                review: "r".into(),
                findings: vec![finding(Some("f1"), FindingKind::Defect)],
            },
            2,
        );
        apply_dispositions(
            &mut cp,
            &SelfReviewFixFile {
                dispositions: vec![DispositionEntry {
                    id: "f1".into(),
                    action: Disposition::Fixed,
                    reason: None,
                }],
            },
        );
        assert_eq!(cp.self_review_ledger[0].fix_attempts, 2);

        // Review re-lists it a second time — now it is a ping-pong.
        update_ledger_from_review(
            &mut cp,
            &SelfReviewFile {
                verdict: ReviewVerdict::Fixable,
                review: "r".into(),
                findings: vec![finding(Some("f1"), FindingKind::Defect)],
            },
            3,
        );
        let pp = ping_pong(&cp).expect("ping-pong");
        assert_eq!(pp.0, "f1");
        assert_eq!(pp.2, 2);
    }

    #[test]
    fn waived_finding_carries_reason_decision_carries_choice() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![
            entry("f1", FindingKind::Defect, FindingStatus::Open),
            entry("f2", FindingKind::Decision, FindingStatus::Open),
        ];
        apply_dispositions(
            &mut cp,
            &SelfReviewFixFile {
                dispositions: vec![
                    DispositionEntry {
                        id: "f1".into(),
                        action: Disposition::Waived,
                        reason: Some("already covered".into()),
                    },
                    DispositionEntry {
                        id: "f2".into(),
                        action: Disposition::Fixed,
                        reason: Some("chose B".into()),
                    },
                ],
            },
        );
        let f1 = &cp.self_review_ledger[0];
        assert_eq!(f1.author_disposition, Some(Disposition::Waived));
        assert_eq!(f1.waive_reason.as_deref(), Some("already covered"));
        let f2 = &cp.self_review_ledger[1];
        assert_eq!(f2.author_disposition, Some(Disposition::Fixed));
        assert_eq!(
            f2.waive_reason.as_deref(),
            Some("chose B"),
            "decision recorded"
        );
    }

    #[test]
    fn fix_file_requires_disposition_per_open_finding() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![
            entry("f1", FindingKind::Defect, FindingStatus::Open),
            entry("f2", FindingKind::Defect, FindingStatus::Open),
        ];
        // Missing f2.
        let err = validate_fix_file(
            &cp,
            &SelfReviewFixFile {
                dispositions: vec![DispositionEntry {
                    id: "f1".into(),
                    action: Disposition::Fixed,
                    reason: None,
                }],
            },
        )
        .unwrap_err();
        assert!(err.contains("`f2`"), "{err}");
        // Waive without a reason.
        let err = validate_fix_file(
            &cp,
            &SelfReviewFixFile {
                dispositions: vec![
                    DispositionEntry {
                        id: "f1".into(),
                        action: Disposition::Fixed,
                        reason: None,
                    },
                    DispositionEntry {
                        id: "f2".into(),
                        action: Disposition::Waived,
                        reason: None,
                    },
                ],
            },
        )
        .unwrap_err();
        assert!(err.contains("reason"), "{err}");
    }

    /// A `decision` marked `fixed` must record the chosen option in `reason`, so
    /// the ledger keeps the decision (issue #212).
    #[test]
    fn decision_fixed_requires_a_recorded_reason() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![entry("f2", FindingKind::Decision, FindingStatus::Open)];
        // `fixed` with no reason is rejected for a decision.
        let err = validate_fix_file(
            &cp,
            &SelfReviewFixFile {
                dispositions: vec![DispositionEntry {
                    id: "f2".into(),
                    action: Disposition::Fixed,
                    reason: None,
                }],
            },
        )
        .unwrap_err();
        assert!(err.contains("decision") && err.contains("`f2`"), "{err}");
        // A blank reason is just as bad.
        assert!(
            validate_fix_file(
                &cp,
                &SelfReviewFixFile {
                    dispositions: vec![DispositionEntry {
                        id: "f2".into(),
                        action: Disposition::Fixed,
                        reason: Some("   ".into()),
                    }],
                },
            )
            .is_err()
        );
        // With the chosen option recorded it passes, and apply keeps it.
        let fix = SelfReviewFixFile {
            dispositions: vec![DispositionEntry {
                id: "f2".into(),
                action: Disposition::Fixed,
                reason: Some("chose B".into()),
            }],
        };
        assert!(validate_fix_file(&cp, &fix).is_ok());
        apply_dispositions(&mut cp, &fix);
        assert_eq!(
            cp.self_review_ledger[0].waive_reason.as_deref(),
            Some("chose B")
        );
    }

    /// The ledger — with all its status — round-trips through the checkpoint
    /// JSON (acceptance: the ledger persists and survives resume). The open
    /// entries are mirrored into `self_review_pending` (rollback safety valve).
    #[test]
    fn ledger_persists_and_mirrors_pending() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![
            LedgerEntry {
                status: FindingStatus::Open,
                author_disposition: Some(Disposition::Waived),
                waive_reason: Some("dup".into()),
                fix_attempts: 1,
                ..entry("f1", FindingKind::Defect, FindingStatus::Open)
            },
            entry("f2", FindingKind::Decision, FindingStatus::Fixed),
        ];
        // The final-fix-in-progress marker persists too (issue #212): the resume
        // route back to the final-fix path depends on it surviving a crash.
        cp.self_review_final_fix_started = true;
        mirror_open_to_pending(&mut cp);
        // Only the open entry is mirrored, and it keeps its id.
        assert_eq!(cp.self_review_pending.len(), 1);
        assert_eq!(cp.self_review_pending[0].id.as_deref(), Some("f1"));

        let json = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.self_review_ledger.len(), 2);
        let f1 = &back.self_review_ledger[0];
        assert_eq!(f1.status, FindingStatus::Open);
        assert_eq!(f1.author_disposition, Some(Disposition::Waived));
        assert_eq!(f1.waive_reason.as_deref(), Some("dup"));
        assert_eq!(f1.fix_attempts, 1);
        assert_eq!(back.self_review_ledger[1].status, FindingStatus::Fixed);
        assert!(back.self_review_final_fix_started);
    }

    /// Forward migration (issue #212): a pre-ledger checkpoint carrying only
    /// `self_review_pending` is promoted into open ledger entries.
    #[test]
    fn pending_promotes_to_ledger() {
        let mut cp = cp_with_title();
        cp.self_review_rounds = 1;
        cp.self_review_pending = vec![Finding {
            id: None,
            kind: FindingKind::Defect,
            path: "src/a.rs".into(),
            line: 3,
            body: "legacy".into(),
            lens: None,
        }];
        promote_pending_to_ledger(&mut cp);
        assert_eq!(cp.self_review_ledger.len(), 1);
        assert_eq!(cp.self_review_ledger[0].status, FindingStatus::Open);
        assert_eq!(cp.self_review_ledger[0].id, "f1");
        assert_eq!(cp.self_review_ledger[0].origin_round, 1);
    }
}
