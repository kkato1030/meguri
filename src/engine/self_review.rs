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
use crate::config::{Config, ProjectConfig};
use crate::gitops;
use crate::store::RunRecord;
use crate::turn::prompts::MEGURI_DIR;
use crate::turn::{TurnOutcome, TurnStatus};

/// Which loop kinds run the internal self-review phase (ADR 0006/0008): worker,
/// planner, and spec-worker opt in via `Flavor::self_reviews()`. The scheduler
/// needs this as a pure function of `loop_kind` (it holds only `RunRecord`, not
/// the `Flavor`), mirroring `collab::supports_advisor_loop_kind`. Keep in sync
/// with the flavors whose `self_reviews()` returns true.
pub fn self_reviews_loop_kind(loop_kind: &str) -> bool {
    loop_kind == crate::engine::worker::KIND
        || loop_kind == crate::engine::planner::KIND
        || loop_kind == crate::engine::spec_worker::KIND
}

/// How many parallel round-1 reviewers this run fans out (issue #214, ADR 0023),
/// or 0 when it takes the single-reviewer path. The scheduler weights slots by
/// this (peak concurrent reviewer agents), so it must agree with what
/// [`self_review`] actually spawns: a self-reviewing loop kind, review enabled,
/// and a non-empty `[[review.reviewers]]`.
pub fn parallel_reviewer_count(cfg: &Config, project: &ProjectConfig, loop_kind: &str) -> usize {
    if !self_reviews_loop_kind(loop_kind) {
        return 0;
    }
    let review = cfg.review_for(project);
    if !review.enabled {
        return 0;
    }
    review.reviewers.len()
}

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
/// A parallel round-1 reviewer reported (issue #214, ADR 0023): carries its
/// profile, lenses, and finding count so `meguri stats review` (#213) can read
/// per-profile unique contribution and waive rate.
pub const EVENT_REVIEWER_REPORTED: &str = "self_review.reviewer_reported";
/// A configured parallel reviewer was dropped (issue #214): its profile failed
/// to detect/resolve or its output was unusable, so the fan-out continued
/// without it (recall from the rest is preserved).
pub const EVENT_REVIEWER_DROPPED: &str = "self_review.reviewer_dropped";
/// The anchor confirmation turn's outcome for a parallel-round `needs_human`
/// (issue #214, ADR 0023 §2): whether the anchor confirmed the escalation.
pub const EVENT_ANCHOR_CONFIRM: &str = "self_review.anchor_confirm";

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

/// Where parallel round-1 reviewer `index` writes its verdict + findings
/// (issue #214): `.meguri/self-review-r<index>.json`, distinct from
/// [`REVIEW_FILE`] so N reviewers never race on one file.
fn parallel_review_file(index: usize) -> String {
    format!(".meguri/self-review-r{index}.json")
}

/// The per-reviewer findings cap injected into each parallel round-1 prompt
/// (issue #214, ADR 0023 §4 / spec §decision 7): keeps the union — and the fix
/// prompt that lists it — from bloating. A constant for now (config later if a
/// wider union proves to dilute the fix turn).
const PARALLEL_FINDINGS_CAP: usize = 5;

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
    /// Which parallel round-1 reviewer profile produced it (issue #214). Never
    /// written by the reviewer — the orchestrator stamps it at merge time. Absent
    /// (and unserialized) on the single-reviewer path, so its checkpoint stays
    /// byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_profile: Option<String>,
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
    /// Which parallel round-1 reviewer profile first surfaced it (issue #214,
    /// ADR 0020): lets `meguri stats review` (#213) attribute unique
    /// contribution and waive rate per profile. `None` (and unserialized) on the
    /// single-reviewer path, keeping that checkpoint byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_profile: Option<String>,
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
    // Parallel round-1 reviewers (issue #214, ADR 0023). Empty → the historical
    // single-reviewer path, byte-for-byte.
    let reviewers = review_cfg.reviewers.clone();
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
        // Round 1 with `[[review.reviewers]]` fans out to parallel reviewers and
        // union-merges (issue #214, ADR 0023); every other case (round 2+, or no
        // reviewers configured) is the single-reviewer turn, unchanged.
        let turn = if cp.self_review_rounds == 0 && !reviewers.is_empty() {
            round1_parallel_review(deps, run, cp, worktree, &base, kind, &lenses, &reviewers)
                .await?
        } else {
            review_turn(deps, run, cp, worktree, &base, kind, &lenses).await?
        };
        let review = match turn {
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
            reviewer_profile: f.reviewer_profile,
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
            reviewer_profile: e.reviewer_profile.clone(),
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
            // Parallel round-1 findings carry the reviewer that surfaced them
            // (issue #214); the single-reviewer path leaves this None.
            reviewer_profile: f.reviewer_profile.clone(),
        });
    }
}

/// Validate a round 2+ review's finding ids against the ledger (issue #212):
/// every non-null id must match a prior finding. An unmatched id is a typo — if
/// it fell through to [`update_ledger_from_review`] it would resolve the finding
/// the reviewer meant to re-list and reset its ping-pong count. The Err text
/// feeds a corrective prompt. (Round 1 has an empty ledger and all-null ids, so
/// this is a no-op there.)
fn validate_review_ids(
    cp: &Checkpoint,
    review: &SelfReviewFile,
) -> std::result::Result<(), String> {
    let known: HashSet<&str> = cp
        .self_review_ledger
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    for f in &review.findings {
        if let Some(id) = &f.id
            && !known.contains(id.as_str())
        {
            return Err(format!(
                "- finding id `{id}` in `{REVIEW_FILE}` matches no prior finding; \
                 re-list an unresolved finding by repeating its exact `id`, or leave \
                 `id` null for a genuinely new finding"
            ));
        }
    }
    Ok(())
}

/// Apply a fix turn's dispositions to the ledger (issue #212): record the
/// author's claim and bump the fix-attempt counter for each open finding. Status
/// stays open — only the next review resolves it. Each id is applied at most
/// once, so a duplicated disposition can't bump `fix_attempts` twice (belt and
/// braces — [`validate_fix_file`] already rejects duplicates).
fn apply_dispositions(cp: &mut Checkpoint, fix: &SelfReviewFixFile) {
    let mut seen: HashSet<&str> = HashSet::new();
    for d in &fix.dispositions {
        if !seen.insert(d.id.as_str()) {
            continue;
        }
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
    // A duplicated id would bump `fix_attempts` more than once per turn (and is
    // ambiguous) — reject it so the author writes exactly one disposition per
    // finding.
    let mut seen: HashSet<&str> = HashSet::new();
    for d in &fix.dispositions {
        if !seen.insert(d.id.as_str()) {
            return Err(format!(
                "- finding `{}` has more than one disposition in `{FIX_FILE}`; \
                 write exactly one entry per finding",
                d.id
            ));
        }
    }
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
            match read_review(worktree) {
                Err(e) => Some(e),
                // A non-null finding id that matches no prior finding is a typo:
                // left unchecked, `update_ledger_from_review` would treat it as a
                // new finding while silently resolving the one the reviewer meant
                // to re-list — and reset its ping-pong count. Reject it here so the
                // reviewer corrects the id.
                Ok(review) => validate_review_ids(cp, &review).err(),
            }
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

/// A resolved parallel round-1 reviewer (issue #214): its concrete profile name
/// (for attribution + launch) and the lenses it applies.
struct ReviewerPlan {
    index: usize,
    profile_name: String,
    lenses: Vec<String>,
}

/// One fanned-out reviewer's classified outcome, computed inside its task so the
/// join stays a simple collection (issue #214).
enum ReviewerTaskOutcome {
    /// Completed with a parsed, valid review.
    Reviewed(SelfReviewFile),
    /// Unusable (turn failed, pane died, or output did not verify) — dropped so
    /// the fan-out continues without it. Carries a reason for the event.
    Dropped(String),
    /// The user asked to stop mid-turn — the whole phase stops.
    Stopped,
}

/// Round 1 with `[[review.reviewers]]` (issue #214, ADR 0023): fan out one
/// review turn per configured reviewer — each under its own lane, per-turn
/// result file, and review file — then union-merge the findings. `needs_human`
/// is not OR'd: any parallel `needs_human` runs a single anchor confirmation
/// turn (ADR 0023 §2). Every reviewer dropping falls back to the single anchor
/// reviewer, so the phase always gets a round-1 review.
#[allow(clippy::too_many_arguments)]
async fn round1_parallel_review(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
    worktree: &Path,
    base: &str,
    kind: Kind,
    lenses: &[String],
    reviewers: &[crate::config::ReviewerConfig],
) -> Result<ReviewTurn> {
    // Drop the base diff once for every reviewer to read; the incremental diff is
    // a round-2+ concept, so there is none here.
    let base_diff = gitops::diff_against_base(worktree, base).await?;
    std::fs::create_dir_all(worktree.join(MEGURI_DIR))?;
    std::fs::write(worktree.join(DIFF_FILE), &base_diff)?;
    let _ = std::fs::remove_file(worktree.join(REVIEW_FILE));
    let head_before = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
    let language = deps.config.language_for(&deps.project).map(str::to_string);

    // The anchor profile (the `self-reviewer` routing profile): the fallback for
    // a reviewer with no `profile`, and the model that runs the needs_human
    // confirmation — divergence heterogeneous, convergence homogeneous
    // (ADR 0023 §3).
    let anchor_profile = crate::routing::resolve(
        &deps.config,
        "self-reviewer",
        &crate::routing::detect_command,
    )?;

    // Resolve each reviewer to a concrete, detectable profile. An undefined or
    // undetectable profile drops that reviewer (event) and the fan-out continues
    // (recall from the rest survives, spec §decision 9).
    let mut plans: Vec<ReviewerPlan> = Vec::new();
    for (index, rc) in reviewers.iter().enumerate() {
        let profile_name = rc.profile.clone().unwrap_or_else(|| anchor_profile.clone());
        let detectable = crate::routing::profile_by_name(&deps.config, &profile_name)
            .map(|p| crate::routing::detect_command(&p.command))
            .unwrap_or(false);
        if !detectable {
            deps.store.emit(
                Some(&run.id),
                EVENT_REVIEWER_DROPPED,
                json!({ "profile": profile_name, "reason": "profile undefined or command not detected" }),
            )?;
            continue;
        }
        let rlenses = rc.lenses.clone().unwrap_or_else(|| lenses.to_vec());
        plans.push(ReviewerPlan {
            index,
            profile_name,
            lenses: rlenses,
        });
    }

    // Every configured reviewer dropped → fall back to the single anchor reviewer
    // (the historical path) so the phase still gets a round-1 review.
    if plans.is_empty() {
        deps.store.emit(
            Some(&run.id),
            EVENT_REVIEWER_DROPPED,
            json!({ "reason": "all reviewers dropped; falling back to single anchor reviewer" }),
        )?;
        return review_turn(deps, run, cp, worktree, base, kind, lenses).await;
    }

    // Fan out: each reviewer runs concurrently under its own lane + isolated
    // result file, writing its own review file. Prompts are built here (holding
    // `cp`), then moved into the tasks.
    let mut set: tokio::task::JoinSet<(usize, String, ReviewerTaskOutcome)> =
        tokio::task::JoinSet::new();
    for plan in &plans {
        let review_file = parallel_review_file(plan.index);
        let _ = std::fs::remove_file(worktree.join(&review_file));
        let prompt = parallel_review_prompt(
            run,
            cp,
            kind,
            &plan.lenses,
            language.as_deref(),
            &review_file,
        );
        let deps = deps.clone();
        let run = run.clone();
        let worktree_buf = worktree.to_path_buf();
        let profile_name = plan.profile_name.clone();
        let index = plan.index;
        let lane_key = format!("{}#{index}", crate::store::LANE_SELF_REVIEW);
        set.spawn(async move {
            let outcome = flow::run_parallel_review_turn(
                &deps,
                &run,
                &worktree_buf,
                Some(&profile_name),
                lane_key,
                "self-review",
                &prompt,
            )
            .await;
            let classified = classify_reviewer_outcome(&worktree_buf, &review_file, outcome);
            (index, profile_name, classified)
        });
    }

    // Collect, keyed by index for a deterministic union order.
    let mut collected: Vec<(usize, String, ReviewerTaskOutcome)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        collected.push(joined?);
    }
    collected.sort_by_key(|(index, _, _)| *index);

    let mut reviews: Vec<(String, SelfReviewFile)> = Vec::new();
    for (_index, profile_name, outcome) in collected {
        match outcome {
            ReviewerTaskOutcome::Stopped => return Ok(ReviewTurn::Stopped),
            ReviewerTaskOutcome::Dropped(reason) => {
                deps.store.emit(
                    Some(&run.id),
                    EVENT_REVIEWER_DROPPED,
                    json!({ "profile": profile_name, "reason": reason }),
                )?;
            }
            ReviewerTaskOutcome::Reviewed(review) => {
                deps.store.emit(
                    Some(&run.id),
                    EVENT_REVIEWER_REPORTED,
                    json!({
                        "profile": profile_name,
                        "verdict": review.verdict,
                        "findings": review.findings.len(),
                    }),
                )?;
                reviews.push((profile_name, review));
            }
        }
    }

    // Every reviewer that ran turned out unusable → fall back to single anchor.
    if reviews.is_empty() {
        return review_turn(deps, run, cp, worktree, base, kind, lenses).await;
    }

    // Trust but verify, once for the whole fan-out: reviews are read-only, so the
    // tree must be pristine and HEAD unmoved. A violation means a reviewer
    // committed/edited — escalate (rare; the read-only assumption is broken).
    let clean = gitops::status_clean(worktree).await?;
    let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
    if !clean || head_now != head_before {
        return Err(NeedsHuman(format!(
            "parallel self-review on issue #{} left the tree dirty or HEAD moved \
             (working tree clean: {clean}, HEAD {head_now} vs {head_before}) — a \
             reviewer wrote to the checkout, which review turns must never do",
            run.issue_number
        ))
        .into());
    }

    // `needs_human` is not OR'd (ADR 0023 §2): if any reviewer flagged it, one
    // anchor confirmation turn decides.
    let concerns: Vec<String> = reviews
        .iter()
        .filter(|(_, r)| r.verdict == ReviewVerdict::NeedsHuman)
        .map(|(p, r)| format!("- ({p}) {}", r.review.trim()))
        .collect();
    if !concerns.is_empty() {
        match anchor_confirm(
            deps,
            run,
            cp,
            worktree,
            kind,
            lenses,
            language.as_deref(),
            &concerns,
        )
        .await?
        {
            AnchorOutcome::Escalate(review) => return Ok(ReviewTurn::Reviewed(review)),
            AnchorOutcome::Stopped => return Ok(ReviewTurn::Stopped),
            AnchorOutcome::Interrupted(r) => return Ok(ReviewTurn::Interrupted(r)),
            AnchorOutcome::Overruled(extra) => {
                // The anchor overruled the escalation; its own findings (if any)
                // join the union, attributed to the anchor profile.
                return Ok(ReviewTurn::Reviewed(merge_reviews(
                    &reviews,
                    Some((anchor_profile.as_str(), extra)),
                )));
            }
        }
    }

    Ok(ReviewTurn::Reviewed(merge_reviews(&reviews, None)))
}

/// Classify one reviewer's turn result inside its task (issue #214): a
/// successful, verifying review is `Reviewed`; a user stop is `Stopped`;
/// anything else (turn failure, pane death, unparseable output) is `Dropped`.
fn classify_reviewer_outcome(
    worktree: &Path,
    review_file: &str,
    outcome: Result<(TurnOutcome, String)>,
) -> ReviewerTaskOutcome {
    match outcome {
        Ok((TurnOutcome::Completed(result), _)) => match result.status {
            TurnStatus::Success => match read_review_at(worktree, review_file) {
                Ok(review) => ReviewerTaskOutcome::Reviewed(review),
                Err(problem) => ReviewerTaskOutcome::Dropped(problem),
            },
            other => ReviewerTaskOutcome::Dropped(format!("turn status {other:?}")),
        },
        Ok((TurnOutcome::Stopped, _)) => ReviewerTaskOutcome::Stopped,
        Ok((TurnOutcome::PaneDied, _)) => {
            ReviewerTaskOutcome::Dropped("pane died during review".into())
        }
        Err(e) => ReviewerTaskOutcome::Dropped(format!("review turn errored: {e}")),
    }
}

/// The anchor confirmation turn's decision (issue #214, ADR 0023 §2).
enum AnchorOutcome {
    /// The anchor confirmed a human is needed — escalate with this review.
    Escalate(SelfReviewFile),
    /// The anchor overruled; its own review (clean or fixable findings) joins the
    /// union and the phase continues.
    Overruled(SelfReviewFile),
    Stopped,
    Interrupted(String),
}

/// Run the single anchor confirmation turn for a parallel-round `needs_human`
/// (issue #214, ADR 0023 §2): the anchor model re-examines the flagged concerns
/// and either confirms escalation (`needs_human`) or overrules it (clean /
/// fixable). One turn regardless of how many reviewers flagged.
#[allow(clippy::too_many_arguments)]
async fn anchor_confirm(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
    worktree: &Path,
    kind: Kind,
    lenses: &[String],
    language: Option<&str>,
    concerns: &[String],
) -> Result<AnchorOutcome> {
    let _ = std::fs::remove_file(worktree.join(REVIEW_FILE));
    let head_before = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
    let prompt = anchor_confirm_prompt(run, cp, kind, lenses, language, concerns);
    let (outcome, _) =
        flow::run_review_turn(deps, run, worktree, "self-review-anchor", &prompt).await?;
    let result = match outcome {
        TurnOutcome::Completed(r) => r,
        TurnOutcome::Stopped => return Ok(AnchorOutcome::Stopped),
        TurnOutcome::PaneDied => {
            return Ok(AnchorOutcome::Interrupted(
                "pane died during anchor confirmation".into(),
            ));
        }
    };
    // A turn that couldn't complete, touched the tree, or wrote an unusable
    // review can't safely overrule an escalation — default to escalating (the
    // needs_human was already flagged), the conservative choice.
    let escalate = |review: String| {
        AnchorOutcome::Escalate(SelfReviewFile {
            verdict: ReviewVerdict::NeedsHuman,
            review,
            findings: Vec::new(),
        })
    };
    if result.status != TurnStatus::Success {
        deps.store.emit(
            Some(&run.id),
            EVENT_ANCHOR_CONFIRM,
            json!({ "confirmed": true, "reason": "anchor turn did not complete" }),
        )?;
        return Ok(escalate(concerns.join("\n")));
    }
    let clean = gitops::status_clean(worktree).await?;
    let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
    if !clean || head_now != head_before {
        deps.store.emit(
            Some(&run.id),
            EVENT_ANCHOR_CONFIRM,
            json!({ "confirmed": true, "reason": "anchor turn modified the tree" }),
        )?;
        return Ok(escalate(concerns.join("\n")));
    }
    let review = match read_review(worktree) {
        Ok(r) => r,
        Err(_) => {
            deps.store.emit(
                Some(&run.id),
                EVENT_ANCHOR_CONFIRM,
                json!({ "confirmed": true, "reason": "anchor review did not verify" }),
            )?;
            return Ok(escalate(concerns.join("\n")));
        }
    };
    let confirmed = review.verdict == ReviewVerdict::NeedsHuman;
    deps.store.emit(
        Some(&run.id),
        EVENT_ANCHOR_CONFIRM,
        json!({ "confirmed": confirmed }),
    )?;
    if confirmed {
        Ok(AnchorOutcome::Escalate(review))
    } else {
        Ok(AnchorOutcome::Overruled(review))
    }
}

/// Union-merge parallel reviews into one `SelfReviewFile` (issue #214, ADR 0023
/// §1): concatenate every non-`needs_human` reviewer's findings in index order,
/// stamping each with the reviewer's profile for attribution (#213). `extra` is
/// the anchor's overruling review, folded in last. Verdict is `fixable` iff the
/// union is non-empty, else `clean`; ids are left null for the ledger to assign.
fn merge_reviews(
    reviews: &[(String, SelfReviewFile)],
    extra: Option<(&str, SelfReviewFile)>,
) -> SelfReviewFile {
    let mut findings: Vec<Finding> = Vec::new();
    let mut prose: Vec<String> = Vec::new();
    let mut fold = |profile: &str, review: &SelfReviewFile| {
        if !review.review.trim().is_empty() {
            prose.push(review.review.trim().to_string());
        }
        for f in &review.findings {
            let mut f = f.clone();
            f.id = None;
            f.reviewer_profile = Some(profile.to_string());
            findings.push(f);
        }
    };
    for (profile, review) in reviews {
        // A needs_human review carries no findings; overruled, its concern lives
        // on only in prose — nothing to fold into the union.
        if review.verdict != ReviewVerdict::NeedsHuman {
            fold(profile, review);
        }
    }
    if let Some((profile, review)) = &extra {
        fold(profile, review);
    }
    let verdict = if findings.is_empty() {
        ReviewVerdict::Clean
    } else {
        ReviewVerdict::Fixable
    };
    SelfReviewFile {
        verdict,
        review: prose.join("\n\n"),
        findings,
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

/// The round-1 prompt for one parallel reviewer (issue #214, ADR 0023). Like the
/// single round-1 review, but it writes to a per-reviewer `review_file` (not the
/// shared [`REVIEW_FILE`]) and carries a per-reviewer findings cap so the union —
/// and the fix prompt that lists it — stays bounded (§decision 7).
fn parallel_review_prompt(
    run: &RunRecord,
    cp: &Checkpoint,
    kind: Kind,
    lenses: &[String],
    language: Option<&str>,
    review_file: &str,
) -> String {
    let subject = match kind {
        Kind::Plan => "spec/ADR",
        Kind::Impl => "implementation",
    };
    format!(
        "You are one of several reviewers self-reviewing this {subject} of issue #{number} in \
         parallel before it is published as a pull request (self-review round 1). The worktree \
         holds the committed work; `{diff}` is its full diff against the base branch.\n\n\
         # Issue: {title}\n\n\
         # Instructions\n\
         - Read the diff at `{diff}`; browse the checked-out files for context as needed.\n\
         {lens_section}\
         - Review the {subject} for correctness, completeness (tests included where the change is \
           code), and fit with the repository's conventions.\n\
         - Report AT MOST {cap} findings — the most important blocking issues you see. Other \
           reviewers cover other ground; a merged union is assembled from everyone.\n\
         - Do NOT modify, commit, or push anything; the review file below is your only deliverable.\n\
         - Write your review to `{review}` as JSON:\n  {schema}\n\
           - \"clean\": nothing must change before this can be published (pure nitpicks do not \
             block; mention them in `review` and leave `findings` empty).\n\
           - \"fixable\": something must change and you can fix it. Every finding is blocking — there \
             is no severity, so keep non-blocking remarks in `review` prose only. Anchor each finding \
             to a line on the NEW side of the diff, leave `id` null (it is assigned for you), and set \
             `kind`: \"defect\" for a bug/omission you fix in code, \"decision\" for an A-or-B you \
             must settle and record in the {subject}.\n\
           - \"needs_human\": something needs a person to decide — an ambiguous requirement, a risky \
             trade-off, a product/design call you cannot make from the code. Explain it in `review` \
             and leave `findings` empty.\n\
         - A completed review is a success regardless of verdict; report \"failure\"/\"needs_human\" \
           as the turn status only when you cannot review at all (the verdict above is the review's \
           conclusion, not the turn's).\
         {lang_section}",
        number = run.issue_number,
        title = cp.issue_title,
        diff = DIFF_FILE,
        review = review_file,
        cap = PARALLEL_FINDINGS_CAP,
        schema = review_schema(),
        lens_section = lens_instruction(kind, lenses),
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

/// The anchor confirmation prompt (issue #214, ADR 0023 §2): a parallel reviewer
/// flagged `needs_human`; the anchor model re-examines the flagged concerns and
/// decides whether a human is genuinely required, writing to [`REVIEW_FILE`].
fn anchor_confirm_prompt(
    run: &RunRecord,
    cp: &Checkpoint,
    kind: Kind,
    lenses: &[String],
    language: Option<&str>,
    concerns: &[String],
) -> String {
    let subject = match kind {
        Kind::Plan => "spec/ADR",
        Kind::Impl => "implementation",
    };
    format!(
        "You are the anchor reviewer confirming a parallel self-review escalation for issue \
         #{number}. One or more parallel reviewers flagged this {subject} as needing a human \
         decision. Your job is to judge whether a human is GENUINELY required, or whether the \
         concern is something the author can just fix.\n\n\
         # Issue: {title}\n\n\
         # Flagged concerns\n{concerns}\n\n\
         # The change\n\
         - `{diff}` — the full diff against the base branch.\n\n\
         # Instructions\n\
         {lens_section}\
         - Judge the flagged concerns against the diff.\n\
         - Do NOT modify, commit, or push anything; the review file below is your only deliverable.\n\
         - Write your review to `{review}` as JSON:\n  {schema}\n\
           - \"needs_human\": you CONFIRM a person must decide — the concern is a real ambiguous \
             requirement, risky trade-off, or product/design call. Explain in `review`, leave \
             `findings` empty; this escalates.\n\
           - \"fixable\": you OVERRULE the escalation — the concern (and anything else blocking) is \
             fixable in code. List those findings (anchor to a NEW-side line, `id` null).\n\
           - \"clean\": you OVERRULE and nothing blocks — publish as is.\
         {lang_section}",
        number = run.issue_number,
        title = cp.issue_title,
        concerns = concerns.join("\n"),
        diff = DIFF_FILE,
        review = REVIEW_FILE,
        schema = review_schema(),
        lens_section = lens_instruction(kind, lenses),
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
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
    read_review_at(worktree, REVIEW_FILE)
}

/// Parse and validate a review file at an arbitrary worktree-relative path
/// (issue #214): the single review uses [`REVIEW_FILE`], each parallel round-1
/// reviewer its own `self-review-r<i>.json`. Same invariants either way.
fn read_review_at(worktree: &Path, rel: &str) -> std::result::Result<SelfReviewFile, String> {
    let raw = std::fs::read_to_string(worktree.join(rel))
        .map_err(|_| format!("- review file `{rel}` does not exist (write it as instructed)"))?;
    let review: SelfReviewFile = serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- review file `{rel}` is not valid JSON ({e}); expected \
             {{\"verdict\": \"clean\" | \"fixable\" | \"needs_human\", \"review\": \"<Markdown>\", \
             \"findings\": [{{\"id\": null, \"kind\": \"defect\"|\"decision\", \"path\": ..., \
             \"line\": ..., \"body\": ...}}]}}"
        )
    })?;
    if review.verdict != ReviewVerdict::Clean && review.review.trim().is_empty() {
        return Err(format!(
            "- verdict is \"{:?}\" but `review` in `{rel}` is empty; \
             a non-clean verdict must explain what must change",
            review.verdict
        ));
    }
    // `fixable ⇔ findings non-empty`, enforced both directions (issue #212).
    match review.verdict {
        ReviewVerdict::Fixable if review.findings.is_empty() => {
            return Err(format!(
                "- verdict is \"fixable\" but `findings` in `{rel}` is empty; a fixable \
                 review must carry at least one line-anchored finding (use \"clean\" if nothing \
                 blocks, or \"needs_human\" if a person must decide)"
            ));
        }
        ReviewVerdict::Clean if !review.findings.is_empty() => {
            return Err(format!(
                "- verdict is \"clean\" but `findings` in `{rel}` is not empty; \
                 a clean review carries no findings — move the remarks into `review` \
                 or change the verdict"
            ));
        }
        ReviewVerdict::NeedsHuman if !review.findings.is_empty() => {
            return Err(format!(
                "- verdict is \"needs_human\" but `findings` in `{rel}` is not empty; \
                 a needs_human review carries no findings — explain the decision in `review`"
            ));
        }
        _ => {}
    }
    for f in &review.findings {
        if f.path.trim().is_empty() || f.line == 0 || f.body.trim().is_empty() {
            return Err(format!(
                "- every `findings` entry in `{rel}` needs a non-empty \
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
            reviewer_profile: None,
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
            reviewer_profile: None,
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

    /// A non-null finding id that matches nothing is a typo — it must be
    /// rejected, not silently resolve the finding the reviewer meant to re-list
    /// (which would also reset ping-pong). Null ids (new findings) are fine.
    #[test]
    fn unknown_review_id_is_rejected() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![entry("f1", FindingKind::Defect, FindingStatus::Open)];

        // Typo: `f2` doesn't exist.
        let review = SelfReviewFile {
            verdict: ReviewVerdict::Fixable,
            review: "r".into(),
            findings: vec![finding(Some("f2"), FindingKind::Defect)],
        };
        let err = validate_review_ids(&cp, &review).unwrap_err();
        assert!(
            err.contains("`f2`") && err.contains("no prior finding"),
            "{err}"
        );

        // Re-listing the real id, or a null id, both pass.
        let ok = SelfReviewFile {
            verdict: ReviewVerdict::Fixable,
            review: "r".into(),
            findings: vec![
                finding(Some("f1"), FindingKind::Defect),
                finding(None, FindingKind::Defect),
            ],
        };
        assert!(validate_review_ids(&cp, &ok).is_ok());
    }

    /// A duplicated disposition id is rejected, and even if it slips through,
    /// `apply_dispositions` bumps `fix_attempts` at most once per id.
    #[test]
    fn duplicate_fix_disposition_is_rejected_and_idempotent() {
        let mut cp = cp_with_title();
        cp.self_review_ledger = vec![entry("f1", FindingKind::Defect, FindingStatus::Open)];
        let dup = SelfReviewFixFile {
            dispositions: vec![
                DispositionEntry {
                    id: "f1".into(),
                    action: Disposition::Fixed,
                    reason: None,
                },
                DispositionEntry {
                    id: "f1".into(),
                    action: Disposition::Fixed,
                    reason: None,
                },
            ],
        };
        assert!(
            validate_fix_file(&cp, &dup)
                .unwrap_err()
                .contains("more than one")
        );
        // Defense in depth: applying it anyway bumps fix_attempts only once.
        apply_dispositions(&mut cp, &dup);
        assert_eq!(cp.self_review_ledger[0].fix_attempts, 1);
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
            reviewer_profile: None,
        }];
        promote_pending_to_ledger(&mut cp);
        assert_eq!(cp.self_review_ledger.len(), 1);
        assert_eq!(cp.self_review_ledger[0].status, FindingStatus::Open);
        assert_eq!(cp.self_review_ledger[0].id, "f1");
        assert_eq!(cp.self_review_ledger[0].origin_round, 1);
    }

    fn review(verdict: ReviewVerdict, findings: Vec<Finding>) -> SelfReviewFile {
        SelfReviewFile {
            verdict,
            review: "prose".into(),
            findings,
        }
    }

    #[test]
    fn merge_unions_findings_in_index_order_and_stamps_reviewer() {
        // Two fixable reviewers + one clean: the union concatenates findings in
        // reviewer order, nulls their ids (the ledger assigns them), and stamps
        // each with the reviewer's profile for #213 attribution.
        let reviews = vec![
            (
                "opus".to_string(),
                review(
                    ReviewVerdict::Fixable,
                    vec![finding(Some("stale"), FindingKind::Defect)],
                ),
            ),
            ("codex".to_string(), review(ReviewVerdict::Clean, vec![])),
            (
                "grok".to_string(),
                review(
                    ReviewVerdict::Fixable,
                    vec![finding(None, FindingKind::Defect)],
                ),
            ),
        ];
        let merged = merge_reviews(&reviews, None);
        assert_eq!(merged.verdict, ReviewVerdict::Fixable);
        assert_eq!(merged.findings.len(), 2);
        assert!(merged.findings.iter().all(|f| f.id.is_none()));
        assert_eq!(merged.findings[0].reviewer_profile.as_deref(), Some("opus"));
        assert_eq!(merged.findings[1].reviewer_profile.as_deref(), Some("grok"));
    }

    #[test]
    fn merge_all_clean_is_clean() {
        let reviews = vec![
            ("a".to_string(), review(ReviewVerdict::Clean, vec![])),
            ("b".to_string(), review(ReviewVerdict::Clean, vec![])),
        ];
        let merged = merge_reviews(&reviews, None);
        assert_eq!(merged.verdict, ReviewVerdict::Clean);
        assert!(merged.findings.is_empty());
    }

    #[test]
    fn merge_drops_needs_human_findings_and_folds_anchor_override() {
        // A needs_human reviewer contributes no findings; when the anchor
        // overrules with its own fixable findings, those join the union under the
        // anchor profile.
        let reviews = vec![(
            "opus".to_string(),
            review(ReviewVerdict::NeedsHuman, vec![]),
        )];
        let extra = review(
            ReviewVerdict::Fixable,
            vec![finding(None, FindingKind::Defect)],
        );
        let merged = merge_reviews(&reviews, Some(("anchor", extra)));
        assert_eq!(merged.verdict, ReviewVerdict::Fixable);
        assert_eq!(merged.findings.len(), 1);
        assert_eq!(
            merged.findings[0].reviewer_profile.as_deref(),
            Some("anchor")
        );
    }

    #[test]
    fn parallel_count_zero_without_reviewers_and_for_non_review_loops() {
        use crate::config::ReviewerConfig;
        let deps = fake_deps();
        let mut cfg = deps.config.clone();
        let project = &deps.project;
        // No reviewers → single path → weight contribution 0.
        assert_eq!(parallel_reviewer_count(&cfg, project, "worker"), 0);
        // Two reviewers on a self-reviewing loop → 2.
        cfg.review.reviewers = vec![ReviewerConfig::default(), ReviewerConfig::default()];
        assert_eq!(parallel_reviewer_count(&cfg, project, "worker"), 2);
        assert_eq!(parallel_reviewer_count(&cfg, project, "planner"), 2);
        // A loop that never self-reviews contributes 0 even with reviewers set.
        assert_eq!(parallel_reviewer_count(&cfg, project, "fixer"), 0);
        // Review disabled → 0.
        cfg.review.enabled = false;
        assert_eq!(parallel_reviewer_count(&cfg, project, "worker"), 0);
    }
}
