//! The Issue Kind reconciler's PR side (ADR 0012). S1 (#221) seeded it as the
//! merge tail — folding `auto_merger` (ADR 0003 / 0009) and `merge_watch`
//! (superseded ADR 0007) into one level-triggered pass **observe → next_step →
//! act**. S3 (#223) folds the fixer family in: the two placeholder Skips S1 left
//! (conflict / red-CI) become real `Step::Agent` arms, plus a review-thread arm,
//! so `fixer` / `ci_fixer` / `conflict_resolver` are now arms of one `next_step`
//! rather than three self-discovering loops.
//!
//! - **observe** is one informer-cache query (`Forge::observe_open_prs`) whose
//!   API cost is measured and emitted (`reconciler.observe_cost`).
//! - **decide** is the pure function [`next_step`]: same [`Snapshot`] ⇒ same
//!   [`Step`]. Every observed state has exactly one owning arm, so a property
//!   test can prove there is no gap (the BEHIND hole) and no double ownership.
//! - **act** runs the chosen [`Op`] itself, or enqueues a fixer-family
//!   [`Arm`]'s recipe as a `queued` run (gated by backoff and the claim marker,
//!   the workqueue of ADR 0012 §6).
//!
//! The BEHIND problem (an armed PR whose base moved and which no loop rescues)
//! is closed by `Op(UpdateBranch)`: the branch is re-based onto its base and,
//! because the arm marker is head-keyed, the next observation sees the moved
//! head as *unarmed* and re-arms it. The re-arm emerges from the level-triggered
//! observation; it is not an explicit second step.

use anyhow::Result;
use serde_json::json;

use super::pr_reviewer::PR_REVIEW_STATUS;
use super::{Deps, canonical_key, is_combined};
use crate::config::{AutoMergeConfig, AutoMergeMode, AutoMergeOptIn, Autonomy};
use crate::forge::{
    self, ArmOutcome, CheckRollup, CheckState, CommitStatusState, MergePolicy, MergeState,
    MergeStateStatus, MergeStrategy, MergeableState, PrObservation, PullRequest,
    UpdateBranchOutcome,
};
use crate::store::parse_ts;

/// Head-branch prefix identifying meguri's own PRs — the merge tail only ever
/// touches branches meguri opened (same guard as the fixer family).
pub const MEGURI_BRANCH_PREFIX: &str = "meguri/";

/// Labels that block *arming* a not-yet-armed PR beyond the human-stop pair:
/// `working` means a run owns the PR, and the two spec-phase labels mean the PR
/// is still a spec under review. `hold` / `needs-human` are handled one level
/// up (they stop both regimes), so they are not repeated here.
const ARM_BLOCKING_LABELS: &[&str] = &[
    forge::LABEL_WORKING,
    forge::LABEL_SPEC_REVIEWING,
    forge::LABEL_SPEC_READY,
];

/// How long an armed PR may sit Blocked-but-readable before the Stuck backstop
/// escalates it (unchanged from the old merge-watch; ADR 0007's generosity).
const STALE_AFTER_SECS: u64 = 24 * 60 * 60;

/// The head-independent prefix of every arm-marker comment. Its presence marks
/// a PR as armed at *some* head (staleness / arm-since); the full head-keyed
/// marker ([`armed_marker`]) marks the *current* head (idempotency / re-arm).
pub const ARMED_MARKER_PREFIX: &str = "<!-- meguri:automerge armed";

/// The head-keyed arm marker embedded in an arm comment. A head is armed iff a
/// comment carries exactly this marker for it, so a moved head (update-branch,
/// or a fresh push) is never seen as armed and is re-evaluated.
pub fn armed_marker(head_sha: &str) -> String {
    format!("{ARMED_MARKER_PREFIX} head={head_sha} -->")
}

/// Whether any comment carries the arm marker for `head_sha` (the current-head
/// idempotency / re-arm key).
fn head_already_armed(comments: &[forge::PrComment], head_sha: &str) -> bool {
    let marker = armed_marker(head_sha);
    comments.iter().any(|c| c.body.contains(&marker))
}

/// Epoch seconds of the earliest arm marker across any head (the arm-since);
/// `None` when no marker parses (then never stale — never escalate on an
/// unreadable time).
fn armed_since_any_head(comments: &[forge::PrComment]) -> Option<u64> {
    comments
        .iter()
        .filter(|c| c.body.contains(ARMED_MARKER_PREFIX))
        .filter_map(|c| parse_ts(&c.created_at))
        .min()
}

/// The tracked issue a PR closes, parsed strictly from the first body line
/// meguri always writes (`flow.rs`: `"Closes #{n}.\n\n..."`). Anything else is
/// None — a PR without both the `meguri/` branch convention and this link is
/// out of scope.
pub fn linked_issue(body: &str) -> Option<i64> {
    body.lines()
        .next()?
        .trim()
        .strip_prefix("Closes #")?
        .strip_suffix('.')?
        .parse::<i64>()
        .ok()
}

/// The arm gate over repository merge settings (ADR 0003 / 0009). Empty result
/// = OK. Shared by the sweep, `meguri watch` startup, and `meguri doctor`.
pub fn validate_policy(cfg: &AutoMergeConfig, policy: &MergePolicy) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    if !policy.allows(cfg.strategy) {
        problems.push(format!(
            "merge strategy `{}` is not allowed by the repository (ADR 0003 forbids \
             falling back to another strategy)",
            cfg.strategy.as_str()
        ));
    }
    if cfg.mode == AutoMergeMode::Native {
        if !policy.auto_merge_allowed {
            problems.push(
                "repository does not allow auto-merge (enable \"Allow auto-merge\" in \
                 the repo's settings, or use `mode = \"orchestrator\"` on private+Free \
                 repos where it cannot be enabled)"
                    .to_string(),
            );
        }
        if cfg.require_branch_protection && !policy.protected_with_required_checks {
            problems.push(
                "base branch has no classic branch protection with required status checks \
                 (set `require_branch_protection = false` to arm without it, e.g. on \
                 rulesets or without an admin token)"
                    .to_string(),
            );
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// First 12 chars of a sha for human-facing text.
fn short_sha(head_sha: &str) -> &str {
    head_sha.get(..12).unwrap_or(head_sha)
}

/// meguri's own light API operations (ADR 0012 §4, Step's `Op` arm). The merge
/// tail launches no agents, so this is the whole Step vocabulary it produces
/// alongside `Wait`. Only the four variants this slice executes exist; the rest
/// (`Finalize`, `EnsureClone`) arrive with their slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Merge base into the head branch — the BEHIND fix (issue #221).
    UpdateBranch,
    /// Arm GitHub-native auto-merge (ADR 0003, native mode).
    ArmAutoMerge,
    /// Merge the PR directly (ADR 0009 orchestrator, or an AlreadyClean finalize).
    MergePr,
    /// Park the PR on `needs-human` (pr-review failed, or a Stuck backstop).
    Escalate,
}

/// A fixer-family arm: a heavy agent recipe the reconciler launches (ADR 0012
/// §4, Step's `Agent` arm). Each maps to a `runs.loop_kind` and its `run_*`
/// entry point; the `dispatch_rank` ordering (ADR 0001 → §5) is
/// conflict-resolver < ci-fixer < fixer (closest to merge first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// A CONFLICTING PR — merge the base and resolve semantically.
    ConflictResolver,
    /// A Blocked PR with a failing required check — diagnose and fix CI.
    CiFixer,
    /// An open review thread awaiting meguri — address the comments.
    Fixer,
}

impl Arm {
    /// The `runs.loop_kind` this arm dispatches to (the recipe's `KIND`).
    pub fn loop_kind(self) -> &'static str {
        match self {
            Arm::ConflictResolver => super::conflict_resolver::KIND,
            Arm::CiFixer => super::ci_fixer::KIND,
            Arm::Fixer => super::fixer::KIND,
        }
    }
}

/// The decision `next_step` returns for one PR. `Agent` launches a fixer-family
/// recipe, `Op` runs a light API operation, `Wait` means the owning arm
/// intentionally stays idle (human stop / pending / policy-disabled), and
/// `Skip` means the state is terminal / not ours / transient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Agent(Arm),
    Op(Op),
    Wait(&'static str),
    Skip(&'static str),
}

/// The pr-review gate's verdict (ADR 0008 §5), pre-reduced into the Snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrReviewGate {
    /// Review disabled, or a success status on the head — arming may proceed.
    Proceed,
    /// Review enabled but the status is absent/pending — wait.
    Wait,
    /// Review enabled and the head's status is a failure — escalate.
    Failed,
}

/// The pure inputs [`next_step`] decides on: no wall-clock, no I/O. The sweep
/// builds it from a [`PrObservation`] plus config; `next_step` maps it to a
/// [`Step`]. Deliberately total so a property test can enumerate it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The PR is still open (terminal PRs need nothing).
    pub open: bool,
    /// The head branch is one meguri opened.
    pub is_meguri_branch: bool,
    /// A human parked/paused the PR (`hold` / `needs-human`) — stops both
    /// regimes and is the escalation idempotency brake.
    pub human_stop: bool,
    /// The *current* head carries an arm marker (routes watch vs arm regime).
    pub current_head_armed: bool,
    /// GitHub's merge snapshot, or `None` when unreadable (transient).
    pub merge: Option<MergeState>,
    /// Armed longer than [`STALE_AFTER_SECS`] (any head) — the Stuck threshold.
    pub stale: bool,
    /// A required check failed (splits a Blocked PR: ci-fixer vs Stuck).
    pub rollup_failure: bool,
    /// The spec worker owns this branch (combined delivery + `spec-ready`); no
    /// fixer-family arm may touch it (`pr_is_touchable`'s spec-ready gate).
    pub spec_worker_owns: bool,
    /// A live author-lane run already owns this issue (any branch-editing loop,
    /// including a fixer-family recipe in flight). The reconciler stays off the
    /// PR while one runs. This is keyed on **run liveness, not the
    /// `meguri:working` label** (f3): a stale label from a crashed run must not
    /// deadlock recovery — a terminal / missing run reads as not-busy, so the
    /// arms, budget escalation, and the stuck backstop all resume.
    pub issue_busy: bool,
    /// A review thread has the ball in meguri's court (unresolved, last comment
    /// not meguri's reply marker) — the Fixer arm's trigger.
    pub awaits_fixer_thread: bool,
    /// The conflict-resolver budget is spent (still-conflicting → escalate).
    pub conflict_exhausted: bool,
    /// The ci-fixer budget is spent (still-red → escalate).
    pub ci_exhausted: bool,
    /// The PR is an arm candidate: linked issue, opted in, no arm-blocking label.
    pub arm_candidate: bool,
    /// An unresolved review thread is open (arm waits on resolution).
    pub has_unresolved_thread: bool,
    /// The pr-review gate verdict.
    pub pr_review: PrReviewGate,
    /// The project runs at `full` autonomy (else a human is the merge gate).
    pub autonomy_full: bool,
    /// Auto-merge is enabled in config (`[pr.auto_merge] enabled`). The fixer
    /// arms run regardless; only the merge tail (arm / merge / update / stuck)
    /// is gated on it, so turning auto-merge off never disables conflict / CI /
    /// thread fixing.
    pub auto_merge_config_enabled: bool,
    /// The repository merge settings pass [`validate_policy`].
    pub policy_ok: bool,
    /// native (arm) vs orchestrator (direct merge).
    pub mode: AutoMergeMode,
}

impl Snapshot {
    /// Whether meguri may perform a base-touching write (arm / update-branch)
    /// on this PR: the same gate arming passes (opt-in / labels / autonomy /
    /// policy), so update-branch never touches a PR arming would not.
    fn can_write(&self) -> bool {
        self.arm_candidate && self.autonomy_full && self.policy_ok
    }
}

/// The pure decision (ADR 0012 §3). Ordering encodes precedence: the fixer
/// family (conflict > ci > threads, by merge proximity) is decided first
/// because a conflicting / red-CI / thread-awaiting PR needs an agent whatever
/// its arm state; then the merge tail (S1) handles BEHIND / arm / merge / stuck.
/// Every observed state is owned by exactly one arm (the `no gap / no double`
/// property).
pub fn next_step(s: &Snapshot) -> Step {
    if !s.open {
        return Step::Skip("terminal (merged/closed)");
    }
    if !s.is_meguri_branch {
        return Step::Skip("not a meguri branch");
    }
    // A human stop is final for every arm, and the durable "already escalated"
    // brake that makes a Stuck / review-failed / budget escalation fire once.
    if s.human_stop {
        return Step::Skip("human stop (hold/needs-human)");
    }
    // Under combined delivery a `spec-ready` PR's branch is the spec worker's
    // (ADR 0008 §6); no fixer-family arm nor the merge tail touches it.
    if s.spec_worker_owns {
        return Step::Skip("spec worker owns the branch");
    }
    // A live author-lane run already owns this issue (a fixer-family recipe in
    // flight, or an external loop like spec_fixer). Stay off it — they share the
    // author pane / worktree. Run-liveness, not the `meguri:working` label, so a
    // stale label from a crashed run cannot deadlock recovery (f3): this gate
    // also fronts the budget escalation and stuck backstop below, so those only
    // fire once nothing is actively working the issue.
    if s.issue_busy {
        return Step::Skip("a live run owns the issue");
    }

    // Fixer family (ADR 0007 supersede completed): the two S1 placeholder Skips
    // become real Agent arms, plus a thread arm. Budget exhaustion parks
    // (needs-human) instead of looping — the #176 order holds because we only
    // reach the conflict/ci escalate while the symptom is still present.
    if let Some(m) = &s.merge {
        if m.mergeable == MergeableState::Conflicting || m.status == MergeStateStatus::Dirty {
            return if s.conflict_exhausted {
                Step::Op(Op::Escalate)
            } else {
                Step::Agent(Arm::ConflictResolver)
            };
        }
        if m.status == MergeStateStatus::Blocked && s.rollup_failure {
            return if s.ci_exhausted {
                Step::Op(Op::Escalate)
            } else {
                Step::Agent(Arm::CiFixer)
            };
        }
    }
    if s.awaits_fixer_thread {
        return Step::Agent(Arm::Fixer);
    }

    // The merge tail (arm / merge / update-branch / stuck) only runs when
    // auto-merge is enabled; the fixer arms above are independent of it.
    if !s.auto_merge_config_enabled {
        return Step::Skip("auto-merge disabled");
    }

    if s.current_head_armed {
        // Watch regime: this head is armed; classify the residual drift
        // (conflict / red-CI already owned by the fixer arms above).
        let Some(m) = &s.merge else {
            return Step::Skip("merge state unreadable (transient)");
        };
        if !m.auto_merge_enabled {
            return Step::Wait("human disabled auto-merge");
        }
        // BEHIND — the hole the old sweeps left open. Close it by re-basing,
        // gated by the same write-eligibility arming needs.
        if m.status == MergeStateStatus::Behind {
            return if s.can_write() {
                Step::Op(Op::UpdateBranch)
            } else {
                Step::Wait("behind, but not eligible to update")
            };
        }
        // Blocked, no failing check, no loop to rescue it, past the threshold:
        // the one class the backstop escalates (Stuck; now behind-free).
        if m.status == MergeStateStatus::Blocked && s.stale {
            return Step::Op(Op::Escalate);
        }
        // Clean / Unstable / pending / not-yet-stale — GitHub merges or waits.
        return Step::Wait("healthy / waiting");
    }

    // Arm regime: the current head is not armed.
    if !s.arm_candidate {
        return Step::Skip("not an arm candidate (label / link / opt-in)");
    }
    if s.has_unresolved_thread {
        return Step::Wait("unresolved review thread");
    }
    // pr-review failure escalates before the autonomy gate: escalation is
    // mode-independent (ADR 0012 §5), so a review-failed head gets its
    // needs-human backstop even when arming is off under `attended`.
    match s.pr_review {
        PrReviewGate::Failed => return Step::Op(Op::Escalate),
        PrReviewGate::Wait => return Step::Wait("pr-review pending"),
        PrReviewGate::Proceed => {}
    }
    if !s.autonomy_full {
        return Step::Skip("autonomy not full (a human merges)");
    }
    if !s.policy_ok {
        return Step::Skip("repository merge policy unmet");
    }
    // A not-yet-armed BEHIND PR is re-based first (closes the orchestrator-mode
    // BEHIND stall too); the next observation arms / merges the fresh head.
    if let Some(m) = &s.merge
        && m.status == MergeStateStatus::Behind
    {
        return Step::Op(Op::UpdateBranch);
    }
    match s.mode {
        AutoMergeMode::Native => Step::Op(Op::ArmAutoMerge),
        AutoMergeMode::Orchestrator => match s.merge.as_ref().map(|m| m.mergeable) {
            Some(MergeableState::Mergeable) => Step::Op(Op::MergePr),
            // Conflicting → conflict-resolver's; Unknown → GitHub still computing.
            _ => Step::Skip("orchestrator: not mergeable yet"),
        },
    }
}

/// Step policy allow-filter (ADR 0026 step policy): a disabled fixer-family
/// arm's `Agent` step becomes `Wait(PolicyDisabled)` — the uniform replacement
/// for the scattered per-loop kill switches. Ownership totality is preserved (a
/// `Wait` is still exactly one owner). Pure, so a property test covers it.
pub fn apply_policy(step: Step, policy: &crate::config::StepPolicyConfig) -> Step {
    match step {
        Step::Agent(arm) if !arm_allowed(arm, policy) => Step::Wait("policy disabled"),
        other => other,
    }
}

fn arm_allowed(arm: Arm, p: &crate::config::StepPolicyConfig) -> bool {
    match arm {
        Arm::ConflictResolver => p.conflict_resolver,
        Arm::CiFixer => p.ci_fixer,
        Arm::Fixer => p.fixer,
    }
}

/// The spec/status-axis signal carrier seam (ADR 0026 signal binding, partial
/// introduction). How the reconciler reads the spec-axis signals it must NOT
/// reconstruct from observation (human stop) and the spec-worker ownership. This
/// slice ships only the [`Labels`] binding — today's behaviour moved behind the
/// seam; a `Markers` binding is future work (the seam is the deliverable).
pub trait SignalCarrier {
    /// A human parked/paused the PR (spec-axis: `hold` / `needs-human`). A
    /// clipped label window reads conservatively as stopped (never miss a stop).
    fn human_stop(&self, pr: &PullRequest, labels_complete: bool) -> bool;
    /// The spec worker owns this branch (combined delivery + `spec-ready`).
    fn spec_worker_owns(&self, pr: &PullRequest, combined: bool) -> bool;
}

/// The default carrier: spec/status signals live on forge labels (ADR 0005).
pub struct Labels;

impl SignalCarrier for Labels {
    fn human_stop(&self, pr: &PullRequest, labels_complete: bool) -> bool {
        pr.has_label(forge::LABEL_HOLD)
            || pr.has_label(forge::LABEL_NEEDS_HUMAN)
            || !labels_complete
    }
    fn spec_worker_owns(&self, pr: &PullRequest, combined: bool) -> bool {
        combined && pr.has_label(forge::LABEL_SPEC_READY)
    }
}

/// Current epoch seconds (`std::time`, same source as `store::now`).
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether this PR is opted into auto-merge: `opt_in = "all"`, the PR carries
/// `meguri:automerge`, or its tracked issue does (the issue-label fallback is
/// the one read left outside the bulk observe, per the spec). Infallible: an
/// unreadable issue reads as "not opted in" (a transient hiccup just defers the
/// arm to the next sweep) — this must never abort the snapshot, or a stuck
/// *armed* PR whose issue is momentarily unreadable would escape the backstop.
async fn opted_in(deps: &Deps, am: &AutoMergeConfig, pr: &PullRequest, issue_number: i64) -> bool {
    if am.opt_in == AutoMergeOptIn::All {
        return true;
    }
    if pr.has_label(forge::LABEL_AUTOMERGE) {
        return true;
    }
    match deps.forge().get_issue(issue_number).await {
        Ok(issue) => issue.has_label(forge::LABEL_AUTOMERGE),
        Err(e) => {
            tracing::debug!("merge-tail: opt-in issue #{issue_number} unreadable: {e:#}");
            false
        }
    }
}

/// Build the pure [`Snapshot`] from one raw observation plus config. The only
/// I/O is the issue-label opt-in fallback (`opted_in`); everything else is a
/// reduction of `obs`.
async fn build_snapshot(
    deps: &Deps,
    am: &AutoMergeConfig,
    obs: &PrObservation,
    policy_ok: bool,
    now: u64,
) -> Result<Snapshot> {
    let pr = &obs.pr;
    let linked = linked_issue(&pr.body);
    let arm_candidate = match linked {
        Some(issue) => {
            !ARM_BLOCKING_LABELS.iter().any(|l| pr.has_label(l))
                && opted_in(deps, am, pr, issue).await
        }
        None => false,
    };
    let pr_review = if !deps.config.review_for(&deps.project).guard.impl_enabled {
        PrReviewGate::Proceed
    } else {
        match obs.pr_review {
            Some(CommitStatusState::Success) => PrReviewGate::Proceed,
            Some(CommitStatusState::Failure) => PrReviewGate::Failed,
            Some(CommitStatusState::Pending) | None => PrReviewGate::Wait,
        }
    };
    let stale = armed_since_any_head(&obs.comments)
        .is_some_and(|since| now.saturating_sub(since) > STALE_AFTER_SECS);
    // Conservative fallback for a clipped observe window: a `hold` / `needs-human`
    // label hidden past the label window must never be missed (it would let a
    // write slip the human stop), and an unresolved thread hidden past the thread
    // window must never be missed (it would let an arm slip the review gate). So
    // an incomplete label set reads as a human stop, and an incomplete thread set
    // reads as "has an unresolved thread". A truncated conversation (the comment
    // page budget / a stalled cursor) also reads as a human stop: an arm or
    // claim marker hidden past the truncation must never be missed, and a
    // pathologically chatty PR parks instead of re-paginating every resync.
    let human_stop = Labels.human_stop(pr, obs.labels_complete) || !obs.comments_complete;
    let has_unresolved_thread =
        obs.review_threads.iter().any(|t| !t.resolved) || !obs.review_threads_complete;
    // The Fixer arm launches a heavy agent, so on an incomplete thread window we
    // do *not* launch on uncertainty (unlike `has_unresolved_thread`, which
    // conservatively blocks arming): a missed awaiting-thread just waits for the
    // next resync's complete observation. `thread_awaits_fixer` reads each
    // thread's last comment (the bulk observe's `comments(last:1)`, §1.5).
    let awaits_fixer_thread = obs.review_threads_complete
        && obs
            .review_threads
            .iter()
            .any(super::fixer::thread_awaits_fixer);
    // A `spec-ready` PR under combined delivery is the spec worker's branch — no
    // fixer-family arm touches it (`pr_is_touchable`'s spec-ready gate).
    let spec_worker_owns = Labels.spec_worker_owns(pr, is_combined(deps));
    let issue = canonical_key(pr);
    let issue_busy = deps
        .store
        .issue_has_active_author_run(&deps.project.id, issue)?;
    // Fixer-family budgets: the arm escalates (parks) once it has spent its
    // successful rounds and the symptom persists (issue #176 order — the arm is
    // only reached while the symptom is still observed).
    let conflict_exhausted =
        deps.store
            .succeeded_run_count(&deps.project.id, super::conflict_resolver::KIND, issue)?
            >= super::conflict_resolver::MAX_RESOLVE_RUNS;
    let ci_exhausted =
        deps.store
            .succeeded_run_count(&deps.project.id, super::ci_fixer::KIND, issue)?
            >= super::ci_fixer::MAX_CI_FIX_RUNS;
    // The ci arm and the Stuck backstop only care about *real* CI: meguri's own
    // `meguri/*` advisory statuses carry no failed-job log and must not spin the
    // ci-fixer (ci_fixer criterion 6).
    let rollup_failure = CheckRollup {
        checks: obs
            .rollup
            .checks
            .iter()
            .filter(|c| !c.name.starts_with("meguri/"))
            .cloned()
            .collect(),
    }
    .state()
        == CheckState::Failure;
    Ok(Snapshot {
        open: pr.state == "open",
        is_meguri_branch: pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX),
        human_stop,
        current_head_armed: head_already_armed(&obs.comments, &pr.head_sha),
        merge: obs.merge.clone(),
        stale,
        rollup_failure,
        spec_worker_owns,
        issue_busy,
        awaits_fixer_thread,
        conflict_exhausted,
        ci_exhausted,
        arm_candidate,
        has_unresolved_thread,
        pr_review,
        autonomy_full: deps.config.autonomy_for(&deps.project) == Autonomy::Full,
        auto_merge_config_enabled: am.enabled,
        policy_ok,
        mode: am.mode,
    })
}

/// Watch-poll sweep: observe every open PR once (informer cache), then drive
/// each meguri PR through `next_step` → act. A per-PR failure warns and is
/// retried next poll; it never aborts the sweep.
pub async fn sweep(deps: &Deps) -> Result<()> {
    if deps.forge.is_none() {
        return Ok(()); // no forge, no PRs (local mode)
    }
    // The sweep runs whenever there is a forge: the fixer family (conflict / CI
    // / thread arms) is independent of auto-merge. Only the merge-tail Ops
    // respect `am.enabled` (gated in `next_step` via `auto_merge_config_enabled`).
    let am = deps.config.pr_for(&deps.project).auto_merge.clone();

    // observe: one bulk query, its API cost measured and recorded (issue #221,
    // acceptance 2). The pr-review context is passed in so the forge stays free
    // of engine vocabulary.
    let observation = deps.forge().observe_open_prs(PR_REVIEW_STATUS).await?;
    let _ = deps.store.emit(
        None,
        "reconciler.observe_cost",
        json!({
            "requests": observation.cost.requests,
            "graphql_cost": observation.cost.graphql_cost,
            "prs": observation.prs.len(),
        }),
    );

    // The repo merge policy is one call per sweep, outside the bulk observe
    // (ADR 0003). Resolved lazily to `policy_ok` on the first meguri PR; an
    // unreadable policy disables writes but never blocks a Stuck escalation.
    let mut policy_ok: Option<bool> = None;
    let now = epoch_now();

    for obs in &observation.prs {
        if let Err(e) = process(deps, &am, obs, &mut policy_ok, now).await {
            tracing::warn!("merge-tail failed for PR #{}: {e:#}", obs.pr.number);
        }
    }

    // Issue Kind per-resync signal act (ADR 0012 §決定4 / finding 3): body-edit
    // re-attention on `implementing` issues, folded out of the scheduler tick's
    // standalone sweep. It never launches an agent nor enqueues — a signal only
    // — so it sits outside the single-arm ownership partition (like
    // `reclaim_stale_claims`), and runs exactly once per resync.
    if let Err(e) = super::reconcile_body_edits::sweep(deps).await {
        tracing::warn!("body-edit reconcile failed for {}: {e:#}", deps.project.id);
    }
    // Separate-delivery plan→impl handoff (ADR 0012 §決定5): a merged spec PR
    // advances its `speccing` issue to `ready`. Folded out of the tick's
    // standalone sweep into the Issue Kind pass (the full `Op(Handoff)` branch of
    // `next_step_issue` folds in with the issue-side observe).
    if let Err(e) = super::plan_handoff::sweep(deps).await {
        tracing::warn!("handoff reconcile failed for {}: {e:#}", deps.project.id);
    }
    // Approved decomposition proposals → child issues + dependencies (ADR 0012
    // §決定4, decompose_materializer → PR-side act). Forge-only, like the merge
    // tail; folded out of the tick's standalone sweep.
    if let Err(e) = super::decompose_materializer::sweep(deps).await {
        tracing::warn!("decompose reconcile failed for {}: {e:#}", deps.project.id);
    }
    Ok(())
}

/// One PR through observe-reduce → next_step → act.
async fn process(
    deps: &Deps,
    am: &AutoMergeConfig,
    obs: &PrObservation,
    policy_ok: &mut Option<bool>,
    now: u64,
) -> Result<()> {
    // Only meguri's own PRs ever act; skip others before any extra read.
    if !obs.pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX) {
        return Ok(());
    }
    // The repo merge policy is only needed by the merge-tail Ops, so resolve it
    // lazily and only when auto-merge is enabled (the fixer arms never use it).
    if am.enabled && policy_ok.is_none() {
        *policy_ok = Some(resolve_policy_ok(deps, am).await);
    }
    let snap = build_snapshot(deps, am, obs, policy_ok.unwrap_or(false), now).await?;
    // Release (tombstone) any of our own claim markers whose run is terminal
    // (ADR 0027 §7) — runs every resync regardless of `next_step`.
    reclaim_stale_claims(deps, obs).await;
    // Clear backoff for any arm whose symptom is *positively* resolved this
    // resync (§4.5) — runs regardless of `next_step`'s single decision.
    clear_resolved_backoffs(deps, obs, &snap)?;
    // Step policy (ADR 0026): a disabled arm's Agent becomes Wait(PolicyDisabled).
    let step = apply_policy(next_step(&snap), &deps.config.reconciler.policy);
    if let Step::Wait("policy disabled") = step {
        deps.store.emit(
            None,
            "reconciler.policy_disabled",
            json!({ "pr": obs.pr.number }),
        )?;
    }
    match step {
        Step::Agent(arm) => enqueue_agent(deps, obs, arm).await?,
        Step::Op(op) => act(deps, am, obs, &snap, op).await?,
        Step::Wait(reason) | Step::Skip(reason) => {
            tracing::debug!("PR #{}: reconciler — {reason}", obs.pr.number);
        }
    }
    Ok(())
}

/// Exponential-backoff base / cap (seconds) for the fixer ping-pong spacing.
const BACKOFF_BASE_SECS: u64 = 5 * 60;
const BACKOFF_CAP_SECS: u64 = 6 * 60 * 60;

/// The head-independent prefix of every claim-marker comment (ADR 0027 / §7).
const CLAIM_MARKER_PREFIX: &str = "<!-- meguri:claim";

/// This instance's id — the claim marker's owner field (`[reconciler] instance`,
/// falling back to the mux session so a single machine needs no config).
fn instance_id(deps: &Deps) -> &str {
    deps.config
        .reconciler
        .instance
        .as_deref()
        .unwrap_or(&deps.config.mux.session)
}

/// The claim marker meguri posts on a PR it is working (arm-agnostic, one per PR).
fn claim_marker(instance: &str, run_id: &str) -> String {
    format!("{CLAIM_MARKER_PREFIX} instance={instance} run={run_id} -->")
}

/// The run id embedded in a claim marker, if the body carries one.
fn parse_claim_run(body: &str) -> Option<&str> {
    let after = body.split(CLAIM_MARKER_PREFIX).nth(1)?;
    let run = after.split("run=").nth(1)?;
    let run = run.split_whitespace().next()?;
    (!run.is_empty()).then_some(run)
}

/// The run id of a *live* claim on this PR, or `None` when unclaimed / stale.
/// Only a self-authored marker is trusted (a third-party forgery is ignored, so
/// no-steal cannot be frozen, f3); a marker whose run is terminal / missing is
/// stale and reclaimable (finding 3). A live run of *any* arm blocks (the
/// family exclusion is per-PR).
fn live_claim(deps: &Deps, obs: &PrObservation) -> Option<String> {
    for c in &obs.comments {
        if !c.viewer_did_author || !c.body.contains(CLAIM_MARKER_PREFIX) {
            continue;
        }
        let Some(run_id) = parse_claim_run(&c.body) else {
            continue;
        };
        match deps.store.get_run(run_id) {
            Ok(Some(run)) if run.status.is_active() => return Some(run_id.to_string()),
            _ => {}
        }
    }
    None
}

/// The body a released claim marker is edited to (ADR 0027 release). It must
/// NOT contain [`CLAIM_MARKER_PREFIX`] so a tombstoned comment is never
/// re-matched as a live claim.
const CLAIM_TOMBSTONE: &str = "🔁 meguri — claim released.";

/// Tombstone self-authored claim markers whose run is terminal (ADR 0027 §7
/// release). This is the reconciler-side release: because the reconciler
/// observes every comment's node id each resync, it releases its own claims
/// here rather than from each recipe's `release_claim` — which also mops up a
/// claim left behind by a crashed run. Best-effort: correctness rides on
/// run-liveness (a stale marker is already ignored by `live_claim`), so an edit
/// failure only emits `reconciler.claim_release_failed` and is retried next
/// resync.
async fn reclaim_stale_claims(deps: &Deps, obs: &PrObservation) {
    for c in &obs.comments {
        if !c.viewer_did_author || c.id.is_empty() || !c.body.contains(CLAIM_MARKER_PREFIX) {
            continue;
        }
        let Some(run_id) = parse_claim_run(&c.body) else {
            continue;
        };
        // A live claim is the active projection — leave it. Only terminal /
        // missing runs are stale and get tombstoned.
        let live = matches!(deps.store.get_run(run_id), Ok(Some(r)) if r.status.is_active());
        if live {
            continue;
        }
        let run_id = run_id.to_string();
        match deps.forge().update_comment(&c.id, CLAIM_TOMBSTONE).await {
            Ok(()) => {
                let _ = deps.store.emit(
                    None,
                    "reconciler.claim_reclaimed",
                    json!({ "pr": obs.pr.number, "run": run_id }),
                );
            }
            Err(e) => {
                tracing::debug!("PR #{}: claim release edit failed: {e:#}", obs.pr.number);
                let _ = deps.store.emit(
                    None,
                    "reconciler.claim_release_failed",
                    json!({ "pr": obs.pr.number, "run": run_id }),
                );
            }
        }
    }
}

/// Post the instance-named claim marker (the forge projection of the family
/// active-run index). Best-effort: the DB index is the atomic authority, so a
/// failed comment does not lose exclusion.
async fn claim_pr(deps: &Deps, pr: i64, run_id: &str) {
    let instance = instance_id(deps).to_string();
    let _ = deps
        .forge()
        .comment_pr(pr, &claim_marker(&instance, run_id))
        .await;
    let _ = deps.store.emit(
        Some(run_id),
        "pr.claimed",
        json!({ "pr": pr, "run": run_id, "instance": instance }),
    );
}

/// The stripped rollup (meguri's own `meguri/*` advisory statuses removed).
fn real_rollup(obs: &PrObservation) -> CheckRollup {
    CheckRollup {
        checks: obs
            .rollup
            .checks
            .iter()
            .filter(|c| !c.name.starts_with("meguri/"))
            .cloned()
            .collect(),
    }
}

/// Drop backoff rows for arms whose symptom is positively resolved (§4.5): a
/// mergeable PR (conflict gone), a green CI rollup (not merely pending), or no
/// thread awaiting meguri. The next symptom opens a fresh episode at exponent 0.
fn clear_resolved_backoffs(deps: &Deps, obs: &PrObservation, snap: &Snapshot) -> Result<()> {
    let p = &deps.project.id;
    let issue = canonical_key(&obs.pr);
    if snap
        .merge
        .as_ref()
        .is_some_and(|m| m.mergeable == MergeableState::Mergeable)
    {
        deps.store
            .clear_backoff(p, issue, super::conflict_resolver::KIND)?;
    }
    if real_rollup(obs).state() == CheckState::Success {
        deps.store.clear_backoff(p, issue, super::ci_fixer::KIND)?;
    }
    if obs.review_threads_complete && !snap.awaits_fixer_thread {
        deps.store.clear_backoff(p, issue, super::fixer::KIND)?;
    }
    Ok(())
}

/// Advance the episode backoff for a still-present symptom (§4.5): open the
/// episode (baseline = current succeeded count, immediately visible) on the
/// first observation, then space each *new* succeeded round once by
/// `base * 2^(n - baseline)` capped at `cap`.
fn advance_backoff(deps: &Deps, issue: i64, arm: Arm) -> Result<()> {
    let p = &deps.project.id;
    let kind = arm.loop_kind();
    let n = deps.store.succeeded_run_count(p, kind, issue)?;
    let now = epoch_now() as i64;
    match deps.store.get_backoff(p, issue, kind)? {
        None => {
            deps.store.upsert_backoff(
                p,
                issue,
                kind,
                crate::store::BackoffRow {
                    baseline_attempt: n,
                    scheduled_attempt: n,
                    next_visible_at: now,
                },
            )?;
        }
        Some(row) if n > row.scheduled_attempt => {
            let k = (n - row.baseline_attempt).max(0) as u32;
            let delay = BACKOFF_BASE_SECS
                .saturating_mul(1u64.checked_shl(k).unwrap_or(u64::MAX))
                .min(BACKOFF_CAP_SECS);
            let next = now.saturating_add(delay as i64);
            deps.store.upsert_backoff(
                p,
                issue,
                kind,
                crate::store::BackoffRow {
                    baseline_attempt: row.baseline_attempt,
                    scheduled_attempt: n,
                    next_visible_at: next,
                },
            )?;
            deps.store.emit(
                None,
                "reconciler.backoff_scheduled",
                json!({ "arm": kind, "issue": issue, "scheduled_attempt": n, "next_visible_at": next }),
            )?;
        }
        Some(_) => {} // this round already spaced — leave next_visible_at
    }
    Ok(())
}

/// Enqueue a fixer-family arm: gate on backoff (`next_visible_at`) and the
/// claim marker's liveness (no-steal / family exclusion, §7), then create a
/// `queued` run keyed by the PR's canonical issue and the arm's `loop_kind`.
/// The scheduler dispatches it (by `dispatch_rank`) through the arm's recipe.
/// The family-wide active-run index (`runs_active_fixer_family`) is the atomic
/// backstop: a unique-index violation just means the work is already in flight,
/// so it is a benign skip.
async fn enqueue_agent(deps: &Deps, obs: &PrObservation, arm: Arm) -> Result<()> {
    let pr = &obs.pr;
    let issue = canonical_key(pr);
    // Advance the episode backoff for this still-present symptom, then gate on
    // it: the PR×arm is spaced out after a fix round that did not resolve the
    // symptom (§4.5).
    advance_backoff(deps, issue, arm)?;
    if deps
        .store
        .backoff_active(&deps.project.id, issue, arm.loop_kind(), epoch_now() as i64)?
    {
        tracing::debug!(
            "PR #{}: reconciler — {} in backoff",
            pr.number,
            arm.loop_kind()
        );
        return Ok(());
    }
    // claim gate (no-steal / family exclusion): a live claim by any instance
    // means the PR is already being worked; a stale claim (terminal run) is
    // reclaimed (§7).
    if let Some(claim) = live_claim(deps, obs) {
        deps.store.emit(
            None,
            "reconciler.claim_skipped",
            json!({ "pr": pr.number, "run": claim }),
        )?;
        return Ok(());
    }
    // A create failure means an active family run already exists (the family /
    // per-loop index) — a benign race; the reconciler retries next resync.
    if let Ok(run) =
        deps.store
            .create_run_for_loop(&deps.project.id, arm.loop_kind(), issue, &pr.title)
    {
        deps.store.emit(
            Some(&run.id),
            "reconciler.enqueued",
            json!({ "arm": arm.loop_kind(), "issue": issue, "pr": pr.number }),
        )?;
        claim_pr(deps, pr.number, &run.id).await;
    }
    Ok(())
}

/// Fetch the repo merge policy once and reduce it to arm-eligibility. An
/// unreadable policy (or one that fails the gate) reads as `false` — writes are
/// disabled, never a hard error (a Stuck escalation needs no policy).
async fn resolve_policy_ok(deps: &Deps, am: &AutoMergeConfig) -> bool {
    match deps
        .forge()
        .merge_policy(&deps.project.default_branch, am.require_branch_protection)
        .await
    {
        Ok(policy) => match validate_policy(am, &policy) {
            Ok(()) => true,
            Err(problems) => {
                tracing::warn!(
                    "merge-tail: repository auto-merge preconditions unmet: {}",
                    problems.join("; ")
                );
                false
            }
        },
        Err(e) => {
            tracing::warn!("merge-tail: merge policy unreadable: {e:#}");
            false
        }
    }
}

/// Run the chosen [`Op`] for one PR.
async fn act(
    deps: &Deps,
    am: &AutoMergeConfig,
    obs: &PrObservation,
    snap: &Snapshot,
    op: Op,
) -> Result<()> {
    match op {
        Op::UpdateBranch => update_branch(deps, &obs.pr).await,
        Op::ArmAutoMerge => arm(deps, am, &obs.pr).await,
        Op::MergePr => merge_directly(deps, am, &obs.pr).await,
        Op::Escalate => {
            // Distinguish the escalation cause from the Snapshot: a spent fixer
            // budget while the symptom persists (conflict / red CI), else the
            // regime split — an armed PR is Stuck, an unarmed one review-failed.
            let conflicting = snap.merge.as_ref().is_some_and(|m| {
                m.mergeable == MergeableState::Conflicting || m.status == MergeStateStatus::Dirty
            });
            let ci = snap
                .merge
                .as_ref()
                .is_some_and(|m| m.status == MergeStateStatus::Blocked)
                && snap.rollup_failure;
            if conflicting && snap.conflict_exhausted {
                escalate_budget_exhausted(deps, &obs.pr, Arm::ConflictResolver).await;
                Ok(())
            } else if ci && snap.ci_exhausted {
                escalate_budget_exhausted(deps, &obs.pr, Arm::CiFixer).await;
                Ok(())
            } else if snap.current_head_armed {
                escalate_stuck(deps, &obs.pr, snap).await
            } else {
                escalate_pr_review_failed(deps, &obs.pr).await;
                Ok(())
            }
        }
    }
}

/// Park a fixer-family PR whose successful rounds are spent but the symptom
/// persists (the old per-loop budget escalations, unified). The `needs-human`
/// label is the durable "escalated" record, so this fires once (`human_stop`
/// then brakes it).
async fn escalate_budget_exhausted(deps: &Deps, pr: &PullRequest, arm: Arm) {
    let (rounds, what, cta) = match arm {
        Arm::ConflictResolver => (
            super::conflict_resolver::MAX_RESOLVE_RUNS,
            "resolved this PR's conflicts",
            "the base keeps re-conflicting",
        ),
        Arm::CiFixer => (
            super::ci_fixer::MAX_CI_FIX_RUNS,
            "pushed CI fixes to this PR",
            "its checks are still failing",
        ),
        Arm::Fixer => (0, "addressed this PR's review comments", "they persist"),
    };
    let comment = super::escalation::pr_needs_human_comment(
        &format!("{what} {rounds} times but {cta}, and needs a human."),
        "解消したら `meguri:needs-human` を外すと再開します(`meguri run --issue N` でも再走できます)。",
        crate::tasks::DEFAULT_ATTACH_HINT,
    );
    super::escalation::escalate_pr(deps, pr.number, &comment).await;
    let _ = deps.store.emit(
        None,
        "reconciler.budget_exhausted",
        json!({ "pr": pr.number, "arm": arm.loop_kind() }),
    );
}

/// Merge base into the head branch (BEHIND fix). Pinned to the observed head so
/// a moved head is rejected (TOCTOU-safe); the re-arm emerges from the next
/// observation seeing the advanced head as unarmed.
async fn update_branch(deps: &Deps, pr: &PullRequest) -> Result<()> {
    match deps.forge().update_branch(pr.number, &pr.head_sha).await? {
        UpdateBranchOutcome::Updated => {
            deps.store.emit(
                None,
                "pr.branch_updated",
                json!({ "pr": pr.number, "head": pr.head_sha }),
            )?;
            tracing::info!(
                "PR #{}: branch updated (behind → base merged in at {})",
                pr.number,
                short_sha(&pr.head_sha)
            );
        }
        // Already up to date or head moved: next sweep re-derives from the
        // fresh head — nothing to record.
        UpdateBranchOutcome::AlreadyUpToDate | UpdateBranchOutcome::HeadMoved => {}
    }
    Ok(())
}

/// Ready (if draft) → arm → marker (ADR 0003). AlreadyClean finalizes with a
/// merge on GitHub's own verdict. The marker is head-keyed, so it is the
/// idempotency key for both paths.
async fn arm(deps: &Deps, am: &AutoMergeConfig, pr: &PullRequest) -> Result<()> {
    if pr.is_draft {
        deps.forge().mark_pr_ready(pr.number).await?;
        deps.store
            .emit(None, "pr.readied", json!({ "pr": pr.number }))?;
    }

    let (body, kind) = match deps
        .forge()
        .enable_auto_merge(pr.number, am.strategy, &pr.head_sha)
        .await?
    {
        ArmOutcome::Armed => (
            armed_comment(am.strategy, &pr.head_sha),
            "pr.automerge_armed",
        ),
        ArmOutcome::AlreadyClean => {
            deps.forge()
                .merge_pr(pr.number, am.strategy, &pr.head_sha)
                .await?;
            (
                merged_comment(am.strategy, &pr.head_sha),
                "pr.automerge_merged",
            )
        }
    };

    deps.forge().comment_pr(pr.number, &body).await?;
    deps.store.emit(
        None,
        kind,
        json!({ "pr": pr.number, "head": pr.head_sha, "strategy": am.strategy.as_str() }),
    )?;
    tracing::info!(
        "PR #{}: {kind} ({} at {})",
        pr.number,
        am.strategy.as_str(),
        short_sha(&pr.head_sha)
    );
    Ok(())
}

/// orchestrator mode (ADR 0009): meguri merges the eligible PR itself. The
/// mergeability gate already fired in `next_step`, so this readies (if draft)
/// and merges pinned to the confirmed head (a moved head is rejected).
async fn merge_directly(deps: &Deps, am: &AutoMergeConfig, pr: &PullRequest) -> Result<()> {
    if pr.is_draft {
        deps.forge().mark_pr_ready(pr.number).await?;
        deps.store
            .emit(None, "pr.readied", json!({ "pr": pr.number }))?;
    }

    deps.forge()
        .merge_pr(pr.number, am.strategy, &pr.head_sha)
        .await?;

    // No arm marker: orchestrator never arms, and the merge closes the PR, so
    // idempotency is the forge's state (ADR 0009).
    deps.forge()
        .comment_pr(
            pr.number,
            &orchestrator_merged_comment(am.strategy, &pr.head_sha),
        )
        .await?;
    deps.store.emit(
        None,
        "pr.automerge_merged",
        json!({ "pr": pr.number, "head": pr.head_sha, "strategy": am.strategy.as_str() }),
    )?;
    tracing::info!(
        "PR #{}: pr.automerge_merged (orchestrator, {} at {})",
        pr.number,
        am.strategy.as_str(),
        short_sha(&pr.head_sha)
    );
    Ok(())
}

/// A review-failed head with auto-merge opted in: park on `needs-human`.
async fn escalate_pr_review_failed(deps: &Deps, pr: &PullRequest) {
    let comment = super::escalation::pr_needs_human_comment(
        &format!(
            "は `{}` の PR review が失敗しているため auto-merge を arm できません。",
            short_sha(&pr.head_sha)
        ),
        "指摘(PR 本文の折り畳み参照)を解消して新しい head を push すると再評価します。",
        crate::tasks::DEFAULT_ATTACH_HINT,
    );
    super::escalation::escalate_pr(deps, pr.number, &comment).await;
    let _ = deps.store.emit(
        None,
        "automerge.pr_review_failed",
        json!({ "pr": pr.number, "head": pr.head_sha }),
    );
}

/// Escalate a Stuck armed PR (Blocked, no failing check, past the threshold).
/// The `needs-human` label is the durable "escalated" record, so this is
/// idempotent without any local state.
async fn escalate_stuck(deps: &Deps, pr: &PullRequest, snap: &Snapshot) -> Result<()> {
    super::escalation::escalate_pr(deps, pr.number, &stuck_comment(snap)).await;
    let status = snap
        .merge
        .as_ref()
        .map(|m| m.status)
        .unwrap_or(MergeStateStatus::Unknown);
    deps.store.emit(
        None,
        "pr.merge_watch_stuck",
        json!({ "pr": pr.number, "status": format!("{status:?}") }),
    )?;
    tracing::info!(
        "PR #{}: merge-tail escalated (stuck armed, mergeStateStatus {:?})",
        pr.number,
        status
    );
    Ok(())
}

/// The comment posted when auto-merge was armed (marker + human line).
fn armed_comment(strategy: MergeStrategy, head_sha: &str) -> String {
    format!(
        "{marker}\n🔁 **meguri** — auto-merge ({strat}) を `{short}` で arm しました。\n\
         required checks が通れば GitHub がマージします。解除したい場合は PR の \
         auto-merge を無効化してください(この head には再 arm しません)。",
        marker = armed_marker(head_sha),
        strat = strategy.as_str(),
        short = short_sha(head_sha),
    )
}

/// The comment posted when GitHub already judged the PR clean and meguri
/// finalized the merge (same marker line, different prose).
fn merged_comment(strategy: MergeStrategy, head_sha: &str) -> String {
    format!(
        "{marker}\n🔁 **meguri** — GitHub が既にマージ可能と判定していたため \
         `{short}` で auto-merge ({strat}) を確定しました。",
        marker = armed_marker(head_sha),
        strat = strategy.as_str(),
        short = short_sha(head_sha),
    )
}

/// The orchestrator-merge audit comment — no arm marker (ADR 0009).
fn orchestrator_merged_comment(strategy: MergeStrategy, head_sha: &str) -> String {
    format!(
        "🔁 **meguri** — ネイティブ auto-merge が使えないため(orchestrator モード) \
         `{short}` を meguri が直接 {strat} マージしました。",
        strat = strategy.as_str(),
        short = short_sha(head_sha),
    )
}

/// The comment posted when the backstop escalates a stuck armed PR.
fn stuck_comment(snap: &Snapshot) -> String {
    let status = snap
        .merge
        .as_ref()
        .map(|m| format!("{:?}", m.status))
        .unwrap_or_else(|| "Unknown".to_string());
    format!(
        "🔁 **meguri** — auto-merge を arm しましたが、この PR は GitHub 側で\
         長時間マージされないまま止まっています(`mergeStateStatus = {status}`)。\
         conflict でも required check の失敗でもないため、conflict-resolver / \
         ci-fixer のどちらも対象にできません。branch protection の設定変更\
         (存在しない required check の要求など)や、必要なレビュー承認待ちが\
         考えられます。人手で確認してください。解消したら `meguri:needs-human` \
         を外すと watch が再開します。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{Forge, PrComment};
    use crate::store::{RunStatus, Store};

    /// A minimal github-mode `Deps` over an in-memory store + fakes.
    fn test_deps() -> Deps {
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
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(crate::forge::fake::FakeForge::default()),
            crate::config::Config::default(),
            project,
        )
    }

    fn obs_with_comment(number: i64, branch: &str, comment: PrComment) -> PrObservation {
        PrObservation {
            pr: PullRequest {
                number,
                title: "t".into(),
                body: String::new(),
                url: String::new(),
                head_branch: branch.into(),
                head_sha: String::new(),
                state: "open".into(),
                is_draft: false,
                labels: vec![],
            },
            merge: None,
            comments: vec![comment],
            review_threads: vec![],
            rollup: CheckRollup::default(),
            pr_review: None,
            labels_complete: true,
            review_threads_complete: true,
            comments_complete: true,
        }
    }

    #[test]
    fn claim_marker_round_trips_and_no_steal_reads_run_liveness() {
        let deps = test_deps();
        // An active run of *any* arm holds the PR (family exclusion / no-steal).
        let run = deps
            .store
            .create_run_for_loop("proj", super::super::fixer::KIND, 9, "t")
            .unwrap();
        let self_marker = PrComment {
            body: claim_marker("me", &run.id),
            viewer_did_author: true,
            ..Default::default()
        };
        assert_eq!(parse_claim_run(&self_marker.body), Some(run.id.as_str()));
        let obs = obs_with_comment(1, "meguri/9-x-abc", self_marker.clone());
        assert_eq!(live_claim(&deps, &obs), Some(run.id.clone()));

        // A third party forging the same marker is ignored (viewer_did_author
        // false) — a forgery cannot freeze no-steal.
        let forged = PrComment {
            viewer_did_author: false,
            ..self_marker.clone()
        };
        assert_eq!(
            live_claim(&deps, &obs_with_comment(1, "meguri/9-x-abc", forged)),
            None
        );

        // Once the run is terminal the self-authored marker is stale → reclaim.
        deps.store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
        assert_eq!(live_claim(&deps, &obs), None);
    }

    #[tokio::test]
    async fn stale_claim_is_tombstoned_live_claim_is_left() {
        use std::sync::Arc;
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
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
        let deps = Deps::with_label_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            crate::config::Config::default(),
            project,
        );
        let pr = forge.push_pr("meguri/9-x-abc", "t (#9)", &[]);
        let run = deps
            .store
            .create_run_for_loop("proj", super::super::fixer::KIND, 9, "t")
            .unwrap();
        // meguri posts its own claim marker (viewer_did_author = true, real id).
        forge
            .comment_pr(pr, &claim_marker("me", &run.id))
            .await
            .unwrap();

        // Live run → the marker is left intact.
        let obs = &forge.observe_open_prs(PR_REVIEW_STATUS).await.unwrap().prs[0];
        reclaim_stale_claims(&deps, obs).await;
        assert!(
            forge
                .pr_comments_of(pr)
                .iter()
                .any(|b| b.contains("meguri:claim")),
            "a live claim must be left as the projection"
        );

        // Terminal run → the marker is tombstoned by its node id.
        deps.store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
        let obs = &forge.observe_open_prs(PR_REVIEW_STATUS).await.unwrap().prs[0];
        reclaim_stale_claims(&deps, obs).await;
        assert!(
            !forge
                .pr_comments_of(pr)
                .iter()
                .any(|b| b.contains("meguri:claim")),
            "a stale claim must be tombstoned"
        );
        assert_eq!(
            deps.store
                .count_events("reconciler.claim_reclaimed")
                .unwrap(),
            1
        );

        // A third party's forged claim marker is never touched (not self-authored).
        forge.add_pr_comment_at(
            pr,
            &claim_marker("attacker", "run-forged"),
            "2026-01-01T00:00:00Z",
        );
        let obs = &forge.observe_open_prs(PR_REVIEW_STATUS).await.unwrap().prs[0];
        reclaim_stale_claims(&deps, obs).await;
        assert!(
            forge
                .pr_comments_of(pr)
                .iter()
                .any(|b| b.contains("run=run-forged")),
            "a third party's marker must never be edited by us"
        );
    }

    #[tokio::test]
    async fn claim_release_failure_is_recorded_not_fatal() {
        use std::sync::Arc;
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        // The fake fails update_comment when issue -1 is in comment_errors.
        forge.comment_errors.lock().unwrap().insert(-1);
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
        let deps = Deps::with_label_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            crate::config::Config::default(),
            project,
        );
        let pr = forge.push_pr("meguri/9-x-abc", "t (#9)", &[]);
        let run = deps
            .store
            .create_run_for_loop("proj", super::super::fixer::KIND, 9, "t")
            .unwrap();
        forge
            .comment_pr(pr, &claim_marker("me", &run.id))
            .await
            .unwrap();
        deps.store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
        let obs = &forge.observe_open_prs(PR_REVIEW_STATUS).await.unwrap().prs[0];
        // Must not panic; the edit failure is recorded and the marker stays
        // (reclaimed next resync). Correctness rides on run-liveness regardless.
        reclaim_stale_claims(&deps, obs).await;
        assert_eq!(
            deps.store
                .count_events("reconciler.claim_release_failed")
                .unwrap(),
            1
        );
        assert!(
            forge
                .pr_comments_of(pr)
                .iter()
                .any(|b| b.contains("meguri:claim"))
        );
    }

    #[test]
    fn backoff_episode_resets_after_positive_resolution() {
        let deps = test_deps();
        let kind = super::super::ci_fixer::KIND;
        let mk_succeeded = || {
            let r = deps
                .store
                .create_run_for_loop("proj", kind, 9, "t")
                .unwrap();
            deps.store
                .update_run_status(&r.id, RunStatus::Succeeded, None)
                .unwrap();
        };

        // Episode 1: first observation (n=0) opens the row, immediately visible.
        advance_backoff(&deps, 9, Arm::CiFixer).unwrap();
        let row0 = deps.store.get_backoff("proj", 9, kind).unwrap().unwrap();
        assert_eq!(row0.baseline_attempt, 0);
        assert_eq!(row0.scheduled_attempt, 0);
        assert!(row0.next_visible_at <= epoch_now() as i64);

        // A fix round succeeds (n=1) and the symptom persists → space once.
        mk_succeeded();
        advance_backoff(&deps, 9, Arm::CiFixer).unwrap();
        let row1 = deps.store.get_backoff("proj", 9, kind).unwrap().unwrap();
        assert_eq!(row1.scheduled_attempt, 1);
        assert!(row1.next_visible_at > epoch_now() as i64, "spaced out");
        // Same round observed again: next_visible_at must not be pushed forward.
        advance_backoff(&deps, 9, Arm::CiFixer).unwrap();
        let row1b = deps.store.get_backoff("proj", 9, kind).unwrap().unwrap();
        assert_eq!(row1b.next_visible_at, row1.next_visible_at);

        // Positive resolution clears the row (green CI).
        let snap = Snapshot {
            merge: None,
            rollup_failure: false,
            awaits_fixer_thread: false,
            ..armed_snapshot()
        };
        let green = obs_with_comment(1, "meguri/9-x-abc", PrComment::default());
        clear_resolved_backoffs(&deps, &green, &snap).unwrap();
        assert!(deps.store.get_backoff("proj", 9, kind).unwrap().is_none());

        // Episode 2 after re-symptom: the exponent resets — baseline is now the
        // all-time succeeded count (1), so k=0 again (immediately visible),
        // not inheriting episode 1's wait.
        advance_backoff(&deps, 9, Arm::CiFixer).unwrap();
        let row2 = deps.store.get_backoff("proj", 9, kind).unwrap().unwrap();
        assert_eq!(row2.baseline_attempt, 1);
        assert_eq!(row2.scheduled_attempt, 1);
        assert!(
            row2.next_visible_at <= epoch_now() as i64,
            "fresh episode: immediate"
        );
    }

    fn merge(status: MergeStateStatus, mergeable: MergeableState, auto: bool) -> MergeState {
        MergeState {
            mergeable,
            status,
            auto_merge_enabled: auto,
        }
    }

    /// A baseline armed-and-healthy snapshot; tests tweak one field.
    fn armed_snapshot() -> Snapshot {
        Snapshot {
            open: true,
            is_meguri_branch: true,
            human_stop: false,
            current_head_armed: true,
            merge: Some(merge(
                MergeStateStatus::Clean,
                MergeableState::Mergeable,
                true,
            )),
            stale: false,
            rollup_failure: false,
            spec_worker_owns: false,
            issue_busy: false,
            awaits_fixer_thread: false,
            conflict_exhausted: false,
            ci_exhausted: false,
            arm_candidate: true,
            has_unresolved_thread: false,
            pr_review: PrReviewGate::Proceed,
            autonomy_full: true,
            auto_merge_config_enabled: true,
            policy_ok: true,
            mode: AutoMergeMode::Native,
        }
    }

    /// A baseline not-yet-armed arm candidate.
    fn arm_snapshot() -> Snapshot {
        Snapshot {
            current_head_armed: false,
            merge: Some(merge(
                MergeStateStatus::Unknown,
                MergeableState::Unknown,
                false,
            )),
            ..armed_snapshot()
        }
    }

    #[test]
    fn linked_issue_parses_the_closes_line_strictly() {
        assert_eq!(linked_issue("Closes #41.\n\nbody"), Some(41));
        assert_eq!(linked_issue("Closes #7.\n"), Some(7));
        assert_eq!(linked_issue("Closes #41\n"), None);
        assert_eq!(linked_issue("Fixes #41.\n"), None);
        assert_eq!(linked_issue("intro\nCloses #41.\n"), None);
        assert_eq!(linked_issue(""), None);
    }

    #[test]
    fn marker_matches_only_its_own_head() {
        let comments = vec![
            PrComment {
                body: "unrelated".into(),
                created_at: String::new(),
                ..Default::default()
            },
            PrComment {
                body: armed_marker("abc123"),
                created_at: String::new(),
                ..Default::default()
            },
        ];
        assert!(head_already_armed(&comments, "abc123"));
        assert!(!head_already_armed(&comments, "def456"));
        assert!(!head_already_armed(&[], "abc123"));
    }

    fn policy(auto: bool, strategies: Vec<MergeStrategy>, protected: bool) -> MergePolicy {
        MergePolicy {
            auto_merge_allowed: auto,
            allowed_strategies: strategies,
            protected_with_required_checks: protected,
        }
    }

    #[test]
    fn validate_policy_accepts_a_fully_configured_repo() {
        let cfg = AutoMergeConfig::default();
        let p = policy(true, vec![MergeStrategy::Squash], true);
        assert!(validate_policy(&cfg, &p).is_ok());
    }

    #[test]
    fn validate_policy_reports_every_missing_precondition() {
        let cfg = AutoMergeConfig::default();
        let p = policy(false, vec![MergeStrategy::Merge], false);
        let problems = validate_policy(&cfg, &p).unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("Allow auto-merge")));
        assert!(problems.iter().any(|p| p.contains("squash")));
        assert!(problems.iter().any(|p| p.contains("branch protection")));
    }

    #[test]
    fn validate_policy_orchestrator_ignores_auto_merge_and_protection() {
        let cfg = AutoMergeConfig {
            mode: AutoMergeMode::Orchestrator,
            require_branch_protection: false,
            ..AutoMergeConfig::default()
        };
        let p = policy(false, vec![MergeStrategy::Squash], false);
        assert!(validate_policy(&cfg, &p).is_ok());
    }

    // --- next_step: watch regime -------------------------------------------

    #[test]
    fn terminal_and_non_meguri_and_human_stop_are_skipped() {
        assert_eq!(
            next_step(&Snapshot {
                open: false,
                ..armed_snapshot()
            }),
            Step::Skip("terminal (merged/closed)")
        );
        assert_eq!(
            next_step(&Snapshot {
                is_meguri_branch: false,
                ..armed_snapshot()
            }),
            Step::Skip("not a meguri branch")
        );
        assert_eq!(
            next_step(&Snapshot {
                human_stop: true,
                ..armed_snapshot()
            }),
            Step::Skip("human stop (hold/needs-human)")
        );
    }

    #[test]
    fn transient_merge_state_is_skipped_never_escalated() {
        let s = Snapshot {
            merge: None,
            stale: true,
            ..armed_snapshot()
        };
        assert_eq!(
            next_step(&s),
            Step::Skip("merge state unreadable (transient)")
        );
    }

    #[test]
    fn human_disabled_auto_merge_waits() {
        let s = Snapshot {
            merge: Some(merge(
                MergeStateStatus::Blocked,
                MergeableState::Mergeable,
                false,
            )),
            stale: true,
            ..armed_snapshot()
        };
        assert_eq!(next_step(&s), Step::Wait("human disabled auto-merge"));
    }

    #[test]
    fn conflict_and_red_ci_are_owned_by_their_arms() {
        // The two S1 placeholder Skips are now real Agent arms (ADR 0007
        // supersede completed).
        let conflict = Snapshot {
            merge: Some(merge(
                MergeStateStatus::Dirty,
                MergeableState::Conflicting,
                true,
            )),
            ..armed_snapshot()
        };
        assert_eq!(next_step(&conflict), Step::Agent(Arm::ConflictResolver));
        // Budget spent while still conflicting → park (needs-human), not loop.
        assert_eq!(
            next_step(&Snapshot {
                conflict_exhausted: true,
                ..conflict
            }),
            Step::Op(Op::Escalate)
        );
        let red = Snapshot {
            merge: Some(merge(
                MergeStateStatus::Blocked,
                MergeableState::Mergeable,
                true,
            )),
            rollup_failure: true,
            ..armed_snapshot()
        };
        assert_eq!(next_step(&red), Step::Agent(Arm::CiFixer));
        assert_eq!(
            next_step(&Snapshot {
                ci_exhausted: true,
                ..red
            }),
            Step::Op(Op::Escalate)
        );
    }

    #[test]
    fn unresolved_thread_awaiting_meguri_is_the_fixer_arm() {
        // A thread with the ball in meguri's court fires the Fixer arm; a
        // parked thread (awaits_fixer false) only blocks arming (Wait).
        let awaiting = Snapshot {
            current_head_armed: false,
            awaits_fixer_thread: true,
            has_unresolved_thread: true,
            merge: Some(merge(
                MergeStateStatus::Unknown,
                MergeableState::Unknown,
                false,
            )),
            ..armed_snapshot()
        };
        assert_eq!(next_step(&awaiting), Step::Agent(Arm::Fixer));
        let parked = Snapshot {
            awaits_fixer_thread: false,
            ..awaiting
        };
        assert_eq!(next_step(&parked), Step::Wait("unresolved review thread"));
    }

    #[test]
    fn spec_worker_owned_branch_is_never_touched() {
        assert_eq!(
            next_step(&Snapshot {
                spec_worker_owns: true,
                awaits_fixer_thread: true,
                ..armed_snapshot()
            }),
            Step::Skip("spec worker owns the branch")
        );
    }

    #[test]
    fn busy_issue_is_skipped_even_with_a_live_symptom() {
        // A PR whose issue has a live author-lane run is left alone even with a
        // live fixer symptom, so the reconciler does not churn (f1). This is
        // keyed on run liveness, not the working label (f3).
        let conflicting = Snapshot {
            issue_busy: true,
            merge: Some(merge(
                MergeStateStatus::Dirty,
                MergeableState::Conflicting,
                true,
            )),
            ..armed_snapshot()
        };
        assert_eq!(
            next_step(&conflicting),
            Step::Skip("a live run owns the issue")
        );
    }

    #[test]
    fn behind_armed_pr_updates_the_branch_closing_the_hole() {
        // The BEHIND regression at the pure-decision level (acceptance 1 / 4).
        let s = Snapshot {
            merge: Some(merge(
                MergeStateStatus::Behind,
                MergeableState::Mergeable,
                true,
            )),
            ..armed_snapshot()
        };
        assert_eq!(next_step(&s), Step::Op(Op::UpdateBranch));
        // Not eligible to write → Wait, never a silent miss.
        let ineligible = Snapshot {
            autonomy_full: false,
            ..s
        };
        assert_eq!(
            next_step(&ineligible),
            Step::Wait("behind, but not eligible to update")
        );
    }

    #[test]
    fn stuck_is_blocked_non_behind_and_stale() {
        let stuck = Snapshot {
            merge: Some(merge(
                MergeStateStatus::Blocked,
                MergeableState::Mergeable,
                true,
            )),
            stale: true,
            ..armed_snapshot()
        };
        assert_eq!(next_step(&stuck), Step::Op(Op::Escalate));
        // Not yet stale → still healthy.
        let fresh = Snapshot {
            stale: false,
            ..stuck
        };
        assert_eq!(next_step(&fresh), Step::Wait("healthy / waiting"));
    }

    // --- next_step: arm regime ---------------------------------------------

    #[test]
    fn native_candidate_arms() {
        assert_eq!(next_step(&arm_snapshot()), Step::Op(Op::ArmAutoMerge));
    }

    #[test]
    fn non_candidate_and_thread_and_pr_review_gates() {
        assert_eq!(
            next_step(&Snapshot {
                arm_candidate: false,
                ..arm_snapshot()
            }),
            Step::Skip("not an arm candidate (label / link / opt-in)")
        );
        assert_eq!(
            next_step(&Snapshot {
                has_unresolved_thread: true,
                ..arm_snapshot()
            }),
            Step::Wait("unresolved review thread")
        );
        assert_eq!(
            next_step(&Snapshot {
                pr_review: PrReviewGate::Failed,
                ..arm_snapshot()
            }),
            Step::Op(Op::Escalate)
        );
        assert_eq!(
            next_step(&Snapshot {
                pr_review: PrReviewGate::Wait,
                ..arm_snapshot()
            }),
            Step::Wait("pr-review pending")
        );
    }

    #[test]
    fn pr_review_failure_escalates_before_the_autonomy_gate() {
        // Under `attended`, a review-failed head still escalates (ADR 0012 §5).
        let s = Snapshot {
            autonomy_full: false,
            pr_review: PrReviewGate::Failed,
            ..arm_snapshot()
        };
        assert_eq!(next_step(&s), Step::Op(Op::Escalate));
        // …but a green head under `attended` is left for a human (no arm).
        let green = Snapshot {
            autonomy_full: false,
            ..arm_snapshot()
        };
        assert_eq!(
            next_step(&green),
            Step::Skip("autonomy not full (a human merges)")
        );
    }

    #[test]
    fn orchestrator_merges_only_when_mergeable() {
        let mergeable = Snapshot {
            mode: AutoMergeMode::Orchestrator,
            merge: Some(merge(
                MergeStateStatus::Clean,
                MergeableState::Mergeable,
                false,
            )),
            ..arm_snapshot()
        };
        assert_eq!(next_step(&mergeable), Step::Op(Op::MergePr));
        // Conflicting is now the ConflictResolver arm (owned before the arm
        // regime); only Unknown stays the orchestrator "not mergeable yet" Skip.
        let conflicting = Snapshot {
            mode: AutoMergeMode::Orchestrator,
            merge: Some(merge(
                MergeStateStatus::Unknown,
                MergeableState::Conflicting,
                false,
            )),
            ..arm_snapshot()
        };
        assert_eq!(next_step(&conflicting), Step::Agent(Arm::ConflictResolver));
        let unknown = Snapshot {
            mode: AutoMergeMode::Orchestrator,
            merge: Some(merge(
                MergeStateStatus::Unknown,
                MergeableState::Unknown,
                false,
            )),
            ..arm_snapshot()
        };
        assert_eq!(
            next_step(&unknown),
            Step::Skip("orchestrator: not mergeable yet")
        );
    }

    #[test]
    fn behind_before_arming_updates_first_both_modes() {
        for mode in [AutoMergeMode::Native, AutoMergeMode::Orchestrator] {
            let s = Snapshot {
                mode,
                merge: Some(merge(
                    MergeStateStatus::Behind,
                    MergeableState::Mergeable,
                    false,
                )),
                ..arm_snapshot()
            };
            assert_eq!(next_step(&s), Step::Op(Op::UpdateBranch), "{mode:?}");
        }
    }

    #[test]
    fn step_policy_disables_only_the_named_arm() {
        use crate::config::StepPolicyConfig;
        let all = StepPolicyConfig::default();
        // Enabled: every step passes through unchanged.
        for step in [
            Step::Agent(Arm::ConflictResolver),
            Step::Agent(Arm::CiFixer),
            Step::Agent(Arm::Fixer),
            Step::Op(Op::UpdateBranch),
            Step::Wait("x"),
            Step::Skip("y"),
        ] {
            assert_eq!(apply_policy(step, &all), step);
        }
        // Disable each arm in turn: only that arm becomes Wait(policy disabled),
        // and non-Agent steps are never touched.
        let cases = [
            (
                Arm::ConflictResolver,
                StepPolicyConfig {
                    conflict_resolver: false,
                    ..all
                },
            ),
            (
                Arm::CiFixer,
                StepPolicyConfig {
                    ci_fixer: false,
                    ..all
                },
            ),
            (
                Arm::Fixer,
                StepPolicyConfig {
                    fixer: false,
                    ..all
                },
            ),
        ];
        for (arm, policy) in cases {
            assert_eq!(
                apply_policy(Step::Agent(arm), &policy),
                Step::Wait("policy disabled")
            );
            // Other arms still pass.
            for other in [Arm::ConflictResolver, Arm::CiFixer, Arm::Fixer] {
                if other != arm {
                    assert_eq!(
                        apply_policy(Step::Agent(other), &policy),
                        Step::Agent(other)
                    );
                }
            }
            // Ownership totality is preserved: a Wait is still exactly one owner.
            assert_eq!(
                apply_policy(Step::Op(Op::MergePr), &policy),
                Step::Op(Op::MergePr)
            );
        }
    }

    #[test]
    fn labels_carrier_reproduces_the_direct_reading() {
        // The signal-binding seam is behaviour-preserving for the Labels
        // binding: for every label combination, the carrier's human_stop /
        // spec_worker_owns equal the direct label reading (the baseline).
        use crate::forge::{LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY};
        let label_sets: &[Vec<String>] = &[
            vec![],
            vec![LABEL_HOLD.into()],
            vec![LABEL_NEEDS_HUMAN.into()],
            vec![LABEL_SPEC_READY.into()],
            vec![LABEL_HOLD.into(), LABEL_SPEC_READY.into()],
        ];
        for labels in label_sets {
            let mut pr = pr_with_labels(labels.clone());
            for &complete in &[true, false] {
                for &combined in &[true, false] {
                    let baseline_stop =
                        pr.has_label(LABEL_HOLD) || pr.has_label(LABEL_NEEDS_HUMAN) || !complete;
                    assert_eq!(Labels.human_stop(&pr, complete), baseline_stop);
                    let baseline_owns = combined && pr.has_label(LABEL_SPEC_READY);
                    assert_eq!(Labels.spec_worker_owns(&pr, combined), baseline_owns);
                }
            }
            pr.labels.clear();
        }
    }

    fn pr_with_labels(labels: Vec<String>) -> crate::forge::PullRequest {
        crate::forge::PullRequest {
            number: 1,
            title: "t".into(),
            body: String::new(),
            url: String::new(),
            head_branch: "meguri/1-x-abc".into(),
            head_sha: String::new(),
            state: "open".into(),
            is_draft: false,
            labels,
        }
    }

    #[test]
    fn ownership_is_total_no_gap_no_double() {
        // Enumerate the observed state space and assert next_step always returns
        // exactly one Step (totality), and that each symptom is owned by exactly
        // the expected arm: BEHIND → UpdateBranch (the closed hole), Conflicting
        // → ConflictResolver, Blocked+red → CiFixer, awaiting thread → Fixer. A
        // missing arm would surface as a panic (unreachable match) or a wrong
        // Step here; a double owner as a precedence-order mismatch.
        use MergeStateStatus::*;
        use MergeableState as M;
        let statuses = [
            Clean, Blocked, Behind, Dirty, Unstable, Draft, HasHooks, Unknown,
        ];
        for &armed in &[true, false] {
            for &status in &statuses {
                for &mergeable in &[M::Mergeable, M::Conflicting, M::Unknown] {
                    for &auto in &[true, false] {
                        for &stale in &[true, false] {
                            for &rollup in &[true, false] {
                                for &awaits in &[true, false] {
                                    for &cx in &[true, false] {
                                        for &cix in &[true, false] {
                                            let s = Snapshot {
                                                current_head_armed: armed,
                                                merge: Some(merge(status, mergeable, auto)),
                                                stale,
                                                rollup_failure: rollup,
                                                awaits_fixer_thread: awaits,
                                                conflict_exhausted: cx,
                                                ci_exhausted: cix,
                                                ..armed_snapshot()
                                            };
                                            let step = next_step(&s);
                                            let conflicting =
                                                mergeable == M::Conflicting || status == Dirty;
                                            // Conflict is the highest-precedence
                                            // symptom (merge proximity).
                                            if conflicting {
                                                assert_eq!(
                                                    step,
                                                    if cx {
                                                        Step::Op(Op::Escalate)
                                                    } else {
                                                        Step::Agent(Arm::ConflictResolver)
                                                    },
                                                    "conflict must be owned: {s:?}"
                                                );
                                            } else if status == Blocked && rollup {
                                                assert_eq!(
                                                    step,
                                                    if cix {
                                                        Step::Op(Op::Escalate)
                                                    } else {
                                                        Step::Agent(Arm::CiFixer)
                                                    },
                                                    "red required CI must be owned: {s:?}"
                                                );
                                            } else if awaits {
                                                assert_eq!(
                                                    step,
                                                    Step::Agent(Arm::Fixer),
                                                    "awaiting thread must be owned: {s:?}"
                                                );
                                            } else if armed && auto && status == Behind {
                                                assert_eq!(
                                                    step,
                                                    Step::Op(Op::UpdateBranch),
                                                    "armed behind must update: {s:?}"
                                                );
                                            }
                                            // Total: a Step was returned.
                                            let _ = step;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Issue-side decider (ADR 0012 slice 4, 決定1). The PR-side above owns open
// meguri PRs; this side owns the pre-PR / non-open-PR issue lifecycle
// (`plan`→planner, `ready`→worker, merged spec PR→handoff), plus a local-task
// decider for local mode. Pure functions with their own property tests; the
// observe/enqueue wiring (single-issue snapshot, issue-wide reservation,
// arm-tagged claim) folds planner/worker/plan_handoff here in a following step.
// The types are issue-scoped (`Issue*`) so they do not disturb the PR-side
// `Snapshot`/`Step`/`Arm`/`Op` above; a later step unifies the vocabulary.
// ===========================================================================

/// How the decider was reached: the normal watch resync, or an explicit manual
/// `meguri run` (ADR 0016). `ManualRun` bypasses the *discovery throttles*
/// (`already_shipped` / cadence window) — a human override — but never the
/// safety gates (human stop / busy / not-before), per finding 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reconcile,
    ManualRun,
}

/// The terminal state of a `speccing` issue's spec PR, read per-issue for the
/// handoff decision (決定5 / f3). Only meaningful for a `speccing` issue that
/// has **no open** meguri PR (an open one is owned by the PR side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecPrState {
    Open,
    Merged,
    ClosedUnmerged,
}

/// A pre-PR / non-open-PR issue arm (ADR 0012 §4, `Agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueArm {
    /// `meguri:plan` → write a spec (planner recipe).
    Planner,
    /// `meguri:ready` → implement (worker recipe).
    Worker,
}

impl IssueArm {
    /// The `runs.loop_kind` this arm dispatches to (the recipe's `KIND`).
    pub fn loop_kind(self) -> &'static str {
        match self {
            IssueArm::Planner => super::planner::KIND,
            IssueArm::Worker => super::worker::KIND,
        }
    }
}

/// A pre-PR / non-open-PR issue Op (ADR 0012 §4, `Op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueOp {
    /// separate delivery: a merged spec PR advances its `speccing` issue to
    /// `ready` (決定5; the old `plan_handoff` sweep, label-only).
    Handoff,
}

/// The decision [`next_step_issue`] returns for one issue identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStep {
    Agent(IssueArm),
    Op(IssueOp),
    Wait(&'static str),
    Skip(&'static str),
}

/// The pure inputs [`next_step_issue`] decides on: one issue's full label set
/// reduced to phase booleans, plus the ownership/serialization gates and the
/// discovery gate predicates for the chosen new-work arm (決定1). Deliberately
/// total so a property test can enumerate it.
#[derive(Debug, Clone, Copy)]
pub struct IssueSnapshot {
    /// A human parked/paused the issue (`hold` / `needs-human`, spec axis).
    /// Respected even under `ManualRun` (finding 2).
    pub human_stop: bool,
    /// The issue has an **open meguri PR** — the ownership boundary hands it to
    /// the PR-side `next_step`; the issue side stays off it (決定1).
    pub has_open_meguri_pr: bool,
    /// A live author-lane run already owns the issue (`issue_has_active_author_run`).
    pub issue_busy: bool,
    /// Phase labels present. Priority `plan` > `ready` > `speccing` >
    /// `implementing`; multiple set (manual drift) still resolves to one arm.
    pub has_plan: bool,
    pub has_ready: bool,
    pub has_speccing: bool,
    pub has_implementing: bool,
    /// For a `speccing` issue with no open PR: its spec PR's terminal state.
    pub spec_pr_state: Option<SpecPrState>,
    /// Discovery gates for the chosen planner/worker arm (現 `LabelTaskSource`
    /// と同じ判定関数の結果を畳んだ純入力):
    /// already shipped (body digest unchanged since a succeeded run).
    pub already_shipped: bool,
    /// not-before is still in the future (fail-closed: honored even under
    /// `ManualRun`, per ADR 0011 / finding 2).
    pub not_before_wait: bool,
    /// A `blocked_by` dependency is still open.
    pub deps_unmet: bool,
    /// The cadence window is exhausted (`limit - consumed <= 0`).
    pub cadence_full: bool,
}

/// The pure decision (ADR 0012 §3, 決定1). Precedence: the ownership /
/// serialization gates first (human stop, open-PR boundary, busy), then the
/// single phase arm by priority, with the discovery gates applied to the chosen
/// new-work arm. Every observed state is owned by exactly one step.
pub fn next_step_issue(s: &IssueSnapshot, mode: Mode) -> IssueStep {
    // Human stop is final for every arm and honored even under ManualRun.
    if s.human_stop {
        return IssueStep::Wait("human stop (hold/needs-human)");
    }
    // Ownership boundary: an open meguri PR is the PR side's (決定1). A stray
    // open-PR speccing issue lands here too, so the boundary is total.
    if s.has_open_meguri_pr {
        return IssueStep::Skip("owned by its open PR");
    }
    // A live author-lane run already owns the issue — stay off it (serialize).
    if s.issue_busy {
        return IssueStep::Skip("a live run owns the issue");
    }
    // Phase priority: exactly one arm. `hold`/`needs-human` was folded into
    // `human_stop` above, so the ladder is plan > ready > speccing > implementing.
    if s.has_plan {
        return gated_new_work(IssueArm::Planner, s, mode);
    }
    if s.has_ready {
        return gated_new_work(IssueArm::Worker, s, mode);
    }
    if s.has_speccing {
        return match s.spec_pr_state {
            Some(SpecPrState::Merged) => IssueStep::Op(IssueOp::Handoff),
            Some(SpecPrState::ClosedUnmerged) => IssueStep::Skip("spec PR closed unmerged"),
            // Open is owned by the PR side (caught by has_open_meguri_pr above);
            // kept total defensively.
            Some(SpecPrState::Open) => IssueStep::Skip("owned by its open PR"),
            None => IssueStep::Skip("speccing: no spec PR yet"),
        };
    }
    if s.has_implementing {
        return IssueStep::Skip("implementing (in progress)");
    }
    IssueStep::Skip("no actionable phase label")
}

/// Apply the discovery gates to a chosen planner/worker arm. not-before and
/// dependency gates hold under both modes (fail-closed); `already_shipped` and
/// the cadence window are the discovery throttles a manual run bypasses.
fn gated_new_work(arm: IssueArm, s: &IssueSnapshot, mode: Mode) -> IssueStep {
    if s.not_before_wait {
        return IssueStep::Wait("not-before (fail-closed)");
    }
    if s.deps_unmet {
        return IssueStep::Wait("blocked by an open dependency");
    }
    if mode == Mode::Reconcile {
        if s.already_shipped {
            return IssueStep::Skip("already shipped, body unchanged");
        }
        if s.cadence_full {
            return IssueStep::Wait("cadence window full");
        }
    }
    IssueStep::Agent(arm)
}

/// A local-task arm — local mode has no planner/PR, so only the worker (決定1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalArm {
    Worker,
}

/// The decision for a local (`TaskKey::Local`) identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStep {
    Agent(LocalArm),
    Wait(&'static str),
    Skip(&'static str),
}

/// The pure inputs for a local task (a subset of [`IssueSnapshot`]: no phase
/// labels / PR / handoff — local mode has none).
#[derive(Debug, Clone, Copy)]
pub struct LocalSnapshot {
    pub human_stop: bool,
    pub issue_busy: bool,
    pub already_shipped: bool,
    pub not_before_wait: bool,
    pub deps_unmet: bool,
    pub cadence_full: bool,
}

/// The pure decision for a local task (決定1, third decider). Same gate ladder
/// as the issue side's worker arm, but the only arm is the worker.
pub fn next_step_local(s: &LocalSnapshot, mode: Mode) -> LocalStep {
    if s.human_stop {
        return LocalStep::Wait("human stop (hold/needs-human)");
    }
    if s.issue_busy {
        return LocalStep::Skip("a live run owns the task");
    }
    if s.not_before_wait {
        return LocalStep::Wait("not-before (fail-closed)");
    }
    if s.deps_unmet {
        return LocalStep::Wait("blocked by an open dependency");
    }
    if mode == Mode::Reconcile {
        if s.already_shipped {
            return LocalStep::Skip("already shipped, body unchanged");
        }
        if s.cadence_full {
            return LocalStep::Wait("cadence window full");
        }
    }
    LocalStep::Agent(LocalArm::Worker)
}

#[cfg(test)]
mod issue_side_tests {
    use super::*;

    /// A baseline: a plain `ready` issue with no gates tripped, not owned by a
    /// PR, not busy. Tests tweak one field.
    fn ready_snapshot() -> IssueSnapshot {
        IssueSnapshot {
            human_stop: false,
            has_open_meguri_pr: false,
            issue_busy: false,
            has_plan: false,
            has_ready: true,
            has_speccing: false,
            has_implementing: false,
            spec_pr_state: None,
            already_shipped: false,
            not_before_wait: false,
            deps_unmet: false,
            cadence_full: false,
        }
    }

    #[test]
    fn phase_priority_picks_exactly_one_arm() {
        // plan wins over ready (manual drift resolves to the highest phase).
        let both = IssueSnapshot {
            has_plan: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&both, Mode::Reconcile),
            IssueStep::Agent(IssueArm::Planner)
        );
        // ready alone → worker.
        assert_eq!(
            next_step_issue(&ready_snapshot(), Mode::Reconcile),
            IssueStep::Agent(IssueArm::Worker)
        );
        assert_eq!(IssueArm::Planner.loop_kind(), super::super::planner::KIND);
        assert_eq!(IssueArm::Worker.loop_kind(), super::super::worker::KIND);
    }

    #[test]
    fn ownership_and_serialization_gates_come_first() {
        // Human stop wins even with a plan label and even under ManualRun.
        let stopped = IssueSnapshot {
            human_stop: true,
            has_plan: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&stopped, Mode::ManualRun),
            IssueStep::Wait("human stop (hold/needs-human)")
        );
        // An open meguri PR hands the issue to the PR side.
        let owned = IssueSnapshot {
            has_open_meguri_pr: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&owned, Mode::Reconcile),
            IssueStep::Skip("owned by its open PR")
        );
        // A live author-lane run serializes.
        let busy = IssueSnapshot {
            issue_busy: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&busy, Mode::Reconcile),
            IssueStep::Skip("a live run owns the issue")
        );
    }

    #[test]
    fn speccing_handoff_matches_the_ownership_rule() {
        // f3: open spec PR → PR side (Skip); merged → Handoff; closed → Skip.
        let base = IssueSnapshot {
            has_ready: false,
            has_speccing: true,
            ..ready_snapshot()
        };
        // An open spec PR sets has_open_meguri_pr, so the boundary catches it
        // before the speccing branch — the issue side never waits on it.
        let open = IssueSnapshot {
            has_open_meguri_pr: true,
            spec_pr_state: Some(SpecPrState::Open),
            ..base
        };
        assert_eq!(
            next_step_issue(&open, Mode::Reconcile),
            IssueStep::Skip("owned by its open PR")
        );
        let merged = IssueSnapshot {
            spec_pr_state: Some(SpecPrState::Merged),
            ..base
        };
        assert_eq!(
            next_step_issue(&merged, Mode::Reconcile),
            IssueStep::Op(IssueOp::Handoff)
        );
        let closed = IssueSnapshot {
            spec_pr_state: Some(SpecPrState::ClosedUnmerged),
            ..base
        };
        assert_eq!(
            next_step_issue(&closed, Mode::Reconcile),
            IssueStep::Skip("spec PR closed unmerged")
        );
    }

    #[test]
    fn discovery_gates_hold_under_reconcile_and_manual_bypasses_throttles() {
        // finding 3: blocked / not-before / cadence-full / already-shipped do not
        // enqueue under Reconcile.
        let shipped = IssueSnapshot {
            already_shipped: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&shipped, Mode::Reconcile),
            IssueStep::Skip("already shipped, body unchanged")
        );
        let full = IssueSnapshot {
            cadence_full: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&full, Mode::Reconcile),
            IssueStep::Wait("cadence window full")
        );
        let blocked = IssueSnapshot {
            deps_unmet: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&blocked, Mode::Reconcile),
            IssueStep::Wait("blocked by an open dependency")
        );
        // finding 2: ManualRun bypasses already_shipped + cadence window …
        assert_eq!(
            next_step_issue(&shipped, Mode::ManualRun),
            IssueStep::Agent(IssueArm::Worker)
        );
        assert_eq!(
            next_step_issue(&full, Mode::ManualRun),
            IssueStep::Agent(IssueArm::Worker)
        );
        // … but not-before stays fail-closed even under ManualRun, and a human
        // stop / dependency block still hold.
        let nb = IssueSnapshot {
            not_before_wait: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&nb, Mode::ManualRun),
            IssueStep::Wait("not-before (fail-closed)")
        );
        assert_eq!(
            next_step_issue(&blocked, Mode::ManualRun),
            IssueStep::Wait("blocked by an open dependency")
        );
    }

    #[test]
    fn local_decider_only_yields_worker_and_respects_the_same_gates() {
        let base = LocalSnapshot {
            human_stop: false,
            issue_busy: false,
            already_shipped: false,
            not_before_wait: false,
            deps_unmet: false,
            cadence_full: false,
        };
        assert_eq!(
            next_step_local(&base, Mode::Reconcile),
            LocalStep::Agent(LocalArm::Worker)
        );
        assert_eq!(
            next_step_local(
                &LocalSnapshot {
                    human_stop: true,
                    ..base
                },
                Mode::ManualRun
            ),
            LocalStep::Wait("human stop (hold/needs-human)")
        );
        // ManualRun bypasses already_shipped for a local task too.
        assert_eq!(
            next_step_local(
                &LocalSnapshot {
                    already_shipped: true,
                    ..base
                },
                Mode::ManualRun
            ),
            LocalStep::Agent(LocalArm::Worker)
        );
    }

    #[test]
    fn ownership_is_total_exactly_one_step_over_the_phase_space() {
        // Enumerate the observed issue-side state space and assert next_step_issue
        // always returns exactly the expected single owning step (no gap, no
        // double) under both modes. The phase powerset × the gate flags × PR
        // ownership × busy is the state space; the expected owner mirrors the
        // precedence ladder.
        for &human_stop in &[true, false] {
            for &has_open_pr in &[true, false] {
                for &busy in &[true, false] {
                    for &plan in &[true, false] {
                        for &ready in &[true, false] {
                            for &speccing in &[true, false] {
                                for &implementing in &[true, false] {
                                    for spec_pr in [
                                        None,
                                        Some(SpecPrState::Open),
                                        Some(SpecPrState::Merged),
                                        Some(SpecPrState::ClosedUnmerged),
                                    ] {
                                        for &shipped in &[true, false] {
                                            for &nb in &[true, false] {
                                                for &deps in &[true, false] {
                                                    for &cadence in &[true, false] {
                                                        for mode in
                                                            [Mode::Reconcile, Mode::ManualRun]
                                                        {
                                                            let s = IssueSnapshot {
                                                                human_stop,
                                                                has_open_meguri_pr: has_open_pr,
                                                                issue_busy: busy,
                                                                has_plan: plan,
                                                                has_ready: ready,
                                                                has_speccing: speccing,
                                                                has_implementing: implementing,
                                                                spec_pr_state: spec_pr,
                                                                already_shipped: shipped,
                                                                not_before_wait: nb,
                                                                deps_unmet: deps,
                                                                cadence_full: cadence,
                                                            };
                                                            assert_eq!(
                                                                next_step_issue(&s, mode),
                                                                expected(&s, mode),
                                                                "{s:?} {mode:?}"
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The reference precedence, independently spelled out, that the property
    /// test above checks `next_step_issue` against.
    fn expected(s: &IssueSnapshot, mode: Mode) -> IssueStep {
        if s.human_stop {
            return IssueStep::Wait("human stop (hold/needs-human)");
        }
        if s.has_open_meguri_pr {
            return IssueStep::Skip("owned by its open PR");
        }
        if s.issue_busy {
            return IssueStep::Skip("a live run owns the issue");
        }
        let gated = |arm: IssueArm| {
            if s.not_before_wait {
                IssueStep::Wait("not-before (fail-closed)")
            } else if s.deps_unmet {
                IssueStep::Wait("blocked by an open dependency")
            } else if mode == Mode::Reconcile && s.already_shipped {
                IssueStep::Skip("already shipped, body unchanged")
            } else if mode == Mode::Reconcile && s.cadence_full {
                IssueStep::Wait("cadence window full")
            } else {
                IssueStep::Agent(arm)
            }
        };
        if s.has_plan {
            gated(IssueArm::Planner)
        } else if s.has_ready {
            gated(IssueArm::Worker)
        } else if s.has_speccing {
            match s.spec_pr_state {
                Some(SpecPrState::Merged) => IssueStep::Op(IssueOp::Handoff),
                Some(SpecPrState::ClosedUnmerged) => IssueStep::Skip("spec PR closed unmerged"),
                Some(SpecPrState::Open) => IssueStep::Skip("owned by its open PR"),
                None => IssueStep::Skip("speccing: no spec PR yet"),
            }
        } else if s.has_implementing {
            IssueStep::Skip("implementing (in progress)")
        } else {
            IssueStep::Skip("no actionable phase label")
        }
    }
}
