//! Merge-watch sweep (auto-merge 2/3, issue #42): poll the PRs #41 armed and
//! detect the ones GitHub has silently stalled, so an armed PR is never left
//! to rot unnoticed (the #35 "silently stranded" problem). Following ADR 0007,
//! **watch is drift detection, not merge authority** — merge-watch never
//! merges and never re-decides "safe to merge"; GitHub does.
//!
//! Crucially, merge-watch does *not* duplicate the fixer loops that landed
//! after this issue was filed. Conflicts are already discovered and resolved
//! by the conflict-resolver loop (#35), and failing required checks by the
//! ci-fixer loop — both independent of arm. merge-watch classifies those but
//! takes **no action**: stamping `meguri:needs-human` on them would evict the
//! PR from those loops' discovery and deadlock the very drift they fix
//! (ADR 0007, decision 1). The one class merge-watch owns is **Stuck**: an
//! armed PR blocked at GitHub for a reason no loop rescues (e.g. a required
//! check added to branch protection that never runs), stuck past a staleness
//! threshold — that is escalated to a human.
//!
//! No local state: watch derives entirely from the forge (the #41 arm marker's
//! `createdAt` for arm-since, the live merge snapshot, and the
//! `meguri:needs-human` label as the "already escalated" brake), so meguri can
//! be killed at any time and recover from GitHub alone (Authority principle).
//! Like the reaper and auto-merger this rides the watch poll as a light API
//! sweep — no run record, no pane.

use anyhow::Result;
use serde_json::json;

use super::Deps;
use super::auto_merger::ARMED_MARKER_PREFIX;
use crate::forge::{self, MergeState, MergeStateStatus, MergeableState, PrComment, PullRequest};
use crate::store::parse_ts;

/// Head-branch prefix identifying meguri's own PRs (watch only ever touches
/// branches meguri opened; the arm marker already implies this, but the guard
/// is cheap and matches the auto-merger / fixer loops).
const MEGURI_BRANCH_PREFIX: &str = "meguri/";

/// How long an armed PR may sit in a stuck-but-readable state before
/// merge-watch escalates it. Generous on purpose: an armed PR legitimately
/// waiting on a human review is also "stalled", and nudging it after a day is
/// the intended behavior, not noise (ADR 0007). A module constant like the
/// fixer loops' `MAX_*_RUNS`; config-ification is left to a later issue.
const STALE_AFTER_SECS: u64 = 24 * 60 * 60;

/// PR lifecycle as watch cares about it. The sweep only ever feeds `Open`
/// (it discovers via `list_open_prs`); `Merged`/`Closed` exist so `classify`
/// can be unit-tested against a terminal snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Open,
    Merged,
    Closed,
}

/// The per-PR inputs `classify` decides on — deliberately free of wall-clock
/// and I/O so the classifier is a pure function (looper's
/// `mergewatch.Classify()`). The sweep populates it; the timing decision is
/// pre-reduced to `stale`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub state: Lifecycle,
    /// GitHub's merge-readiness snapshot, or `None` when the forge call failed
    /// (the TransientError signal — never escalate on it).
    pub merge: Option<MergeState>,
    /// Whether the head's check rollup is FAILURE. Only consulted for a
    /// `Blocked` PR, to split "a required check failed" (ci-fixer's) from
    /// "blocked with no failing check" (Stuck candidate).
    pub rollup_failure: bool,
    /// Whether the PR has been armed longer than [`STALE_AFTER_SECS`].
    pub stale: bool,
}

/// What merge-watch decided about one armed PR's current GitHub snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchClass {
    /// The PR merged (or was closed) — watch is done; the reaper reclaims the
    /// worktree once the issue closes. No action.
    Merged,
    /// Conflicting — the conflict-resolver loop owns it. No action.
    Conflict,
    /// A required check failed — the ci-fixer loop owns it. No action.
    RedCI,
    /// Mergeable, waiting on required review, or only non-required checks
    /// failing — GitHub will merge (or is legitimately waiting). No action.
    Healthy,
    /// Arm marker present but auto-merge is off and the PR did not merge — a
    /// human disabled it. Back off silently.
    HumanDisabled,
    /// The snapshot could not be read (429/5xx). No action; retry next sweep.
    Transient,
    /// Armed, readable, none of the above, and stuck past the staleness
    /// threshold with no loop to rescue it — the one class watch escalates.
    Stuck,
}

impl WatchClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Conflict => "conflict",
            Self::RedCI => "red-ci",
            Self::Healthy => "healthy",
            Self::HumanDisabled => "human-disabled",
            Self::Transient => "transient",
            Self::Stuck => "stuck",
        }
    }
}

/// The classifier (ADR 0007). Ordering encodes the precedence: terminal wins,
/// then an unreadable snapshot is always Transient (never escalated), then the
/// human's decision (auto-merge disabled) is final, then the fixer-owned drift
/// classes, and only a readable, armed, non-fixer-owned, stale PR is Stuck.
pub fn classify(snap: &Snapshot) -> WatchClass {
    // Terminal: merged or otherwise closed — watch ends whatever else is true.
    if snap.state != Lifecycle::Open {
        return WatchClass::Merged;
    }
    // Snapshot unreadable → Transient. Never fold into Stuck: while we cannot
    // read the state we cannot tell Conflict (conflict-resolver's) from RedCI
    // (ci-fixer's), and a needs-human stamp here would deadlock them (ADR 0007).
    let Some(m) = &snap.merge else {
        return WatchClass::Transient;
    };
    // The human's decision is final: arm marker present but auto-merge off.
    if !m.auto_merge_enabled {
        return WatchClass::HumanDisabled;
    }
    // Conflicts → the conflict-resolver loop owns it.
    if m.mergeable == MergeableState::Conflicting || m.status == MergeStateStatus::Dirty {
        return WatchClass::Conflict;
    }
    // A failing required check surfaces as BLOCKED + a red rollup → ci-fixer's.
    if m.status == MergeStateStatus::Blocked && snap.rollup_failure {
        return WatchClass::RedCI;
    }
    // Blocked/behind with no failing check and no loop to rescue it: the
    // backstop, but only once it has been stuck past the threshold. A
    // non-required check failing shows as UNSTABLE (mergeable) and never
    // reaches here — GitHub merges it, so watch leaves it alone.
    if matches!(
        m.status,
        MergeStateStatus::Blocked | MergeStateStatus::Behind
    ) && snap.stale
    {
        return WatchClass::Stuck;
    }
    // Clean / Unstable / pending / not-yet-stale → healthy or still waiting.
    WatchClass::Healthy
}

/// Whether any comment carries the #41 arm marker (any head).
fn is_armed(comments: &[PrComment]) -> bool {
    comments
        .iter()
        .any(|c| c.body.contains(ARMED_MARKER_PREFIX))
}

/// Epoch seconds of the earliest armed-marker comment with a parseable
/// `createdAt` — the arm-since. `None` when no marker parses (watch then never
/// treats the PR as stale: conservative, never escalate on unreadable time).
fn armed_since(comments: &[PrComment]) -> Option<u64> {
    comments
        .iter()
        .filter(|c| c.body.contains(ARMED_MARKER_PREFIX))
        .filter_map(|c| parse_ts(&c.created_at))
        .min()
}

/// Current epoch seconds (`std::time`, same source as `store::now`).
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Watch-poll sweep: classify every armed open meguri PR and escalate the
/// Stuck ones. A per-PR failure warns and is retried next poll; it never
/// aborts the sweep (same shape as the auto-merger).
pub async fn sweep(deps: &Deps) -> Result<()> {
    // No forge, no armed PRs to watch (local mode) — same guard as the
    // auto-merger sweep.
    if deps.forge.is_none() {
        return Ok(());
    }
    // Gated on the same switch as the auto-merger: with auto-merge off there
    // are no fresh arms, and lingering markers are not ours to police.
    if !deps.config.pr_for(&deps.project).auto_merge.enabled {
        return Ok(());
    }
    let now = epoch_now();
    for pr in deps.forge().list_open_prs().await? {
        if let Err(e) = process_pr(deps, &pr, now).await {
            tracing::warn!("merge-watch failed for PR #{}: {e:#}", pr.number);
        }
    }
    Ok(())
}

/// One PR through the watch classification. Only genuine forge failures (other
/// than the merge-state read, which is the Transient signal) return `Err`.
async fn process_pr(deps: &Deps, pr: &PullRequest, now: u64) -> Result<()> {
    // meguri's own PRs only.
    if !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX) {
        return Ok(());
    }
    // Already escalated (idempotency brake, like the fixer loops) or paused by
    // a human: leave it be. This is also what keeps a Stuck escalation from
    // re-commenting every sweep — the label we add makes the next sweep skip.
    if pr.has_label(forge::LABEL_NEEDS_HUMAN) || pr.has_label(forge::LABEL_HOLD) {
        return Ok(());
    }
    // Watch only PRs #41 armed.
    let comments = deps.forge().pr_comments_meta(pr.number).await?;
    if !is_armed(&comments) {
        return Ok(());
    }
    // The merge snapshot; a forge error here is the TransientError signal.
    let merge = match deps.forge().pr_merge_state(pr.number).await {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::debug!(
                "merge-watch: PR #{} merge-state unreadable: {e:#}",
                pr.number
            );
            None
        }
    };
    // The rollup only matters for splitting a Blocked PR (RedCI vs Stuck), so
    // fetch it only then — one extra API call, and only for blocked PRs.
    let rollup_failure = match &merge {
        Some(m) if m.status == MergeStateStatus::Blocked => deps
            .forge()
            .pr_check_rollup(pr.number)
            .await
            .map(|r| r.state() == forge::CheckState::Failure)
            .unwrap_or(false),
        _ => false,
    };
    let stale =
        armed_since(&comments).is_some_and(|since| now.saturating_sub(since) > STALE_AFTER_SECS);
    let snap = Snapshot {
        state: Lifecycle::Open,
        merge,
        rollup_failure,
        stale,
    };

    match classify(&snap) {
        WatchClass::Stuck => escalate(deps, pr, &snap).await?,
        // Every other class is a deliberate no-op: the fixer loops own
        // Conflict/RedCI, the human owns HumanDisabled, Transient retries next
        // sweep, and Merged/Healthy need nothing (ADR 0007).
        other => tracing::debug!(
            "merge-watch: PR #{} classified {} — no action",
            pr.number,
            other.as_str()
        ),
    }
    Ok(())
}

/// Escalate a Stuck PR: `meguri:needs-human` + one explanatory comment. The
/// label is the durable "escalated" record — subsequent sweeps skip it — so
/// this is idempotent without any local state.
async fn escalate(deps: &Deps, pr: &PullRequest, snap: &Snapshot) -> Result<()> {
    deps.forge()
        .add_pr_label(pr.number, forge::LABEL_NEEDS_HUMAN)
        .await?;
    deps.forge()
        .comment_pr(pr.number, &stuck_comment(snap))
        .await?;
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
        "PR #{}: merge-watch escalated (stuck armed, mergeStateStatus {:?})",
        pr.number,
        status
    );
    Ok(())
}

/// The comment posted when watch escalates a stuck armed PR.
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

    fn merge(status: MergeStateStatus, mergeable: MergeableState, auto: bool) -> MergeState {
        MergeState {
            mergeable,
            status,
            auto_merge_enabled: auto,
        }
    }

    fn open(merge: Option<MergeState>, rollup_failure: bool, stale: bool) -> Snapshot {
        Snapshot {
            state: Lifecycle::Open,
            merge,
            rollup_failure,
            stale,
        }
    }

    #[test]
    fn merged_or_closed_is_terminal() {
        for state in [Lifecycle::Merged, Lifecycle::Closed] {
            let snap = Snapshot {
                state,
                merge: Some(merge(
                    MergeStateStatus::Clean,
                    MergeableState::Mergeable,
                    true,
                )),
                rollup_failure: false,
                stale: true,
            };
            assert_eq!(classify(&snap), WatchClass::Merged);
        }
    }

    #[test]
    fn unreadable_snapshot_is_transient_even_when_stale() {
        // The core invariant: no snapshot → never Stuck, whatever the age.
        assert_eq!(classify(&open(None, false, true)), WatchClass::Transient);
        assert_eq!(classify(&open(None, false, false)), WatchClass::Transient);
    }

    #[test]
    fn auto_merge_off_is_human_disabled() {
        // HumanDisabled dominates conflict/blocked: the human's call is final.
        let snap = open(
            Some(merge(
                MergeStateStatus::Dirty,
                MergeableState::Conflicting,
                false,
            )),
            true,
            true,
        );
        assert_eq!(classify(&snap), WatchClass::HumanDisabled);
    }

    #[test]
    fn conflicting_defers_to_conflict_resolver() {
        let by_status = open(
            Some(merge(
                MergeStateStatus::Dirty,
                MergeableState::Unknown,
                true,
            )),
            false,
            true,
        );
        assert_eq!(classify(&by_status), WatchClass::Conflict);
        // Either signal (mergeStateStatus DIRTY or mergeable CONFLICTING) counts.
        let by_mergeable = open(
            Some(merge(
                MergeStateStatus::Unknown,
                MergeableState::Conflicting,
                true,
            )),
            false,
            true,
        );
        assert_eq!(classify(&by_mergeable), WatchClass::Conflict);
    }

    #[test]
    fn blocked_with_failing_required_check_defers_to_ci_fixer() {
        let snap = open(
            Some(merge(
                MergeStateStatus::Blocked,
                MergeableState::Mergeable,
                true,
            )),
            true, // rollup FAILURE
            true,
        );
        assert_eq!(classify(&snap), WatchClass::RedCI);
    }

    #[test]
    fn unstable_non_required_failure_is_healthy_not_redci() {
        // A non-required check failing → GitHub reports UNSTABLE (mergeable) and
        // still merges: watch must not treat it as RedCI (accept-criterion).
        let snap = open(
            Some(merge(
                MergeStateStatus::Unstable,
                MergeableState::Mergeable,
                true,
            )),
            true, // even with a red rollup, UNSTABLE is not BLOCKED
            true,
        );
        assert_eq!(classify(&snap), WatchClass::Healthy);
    }

    #[test]
    fn blocked_without_failing_check_is_stuck_only_when_stale() {
        let base = |stale| {
            open(
                Some(merge(
                    MergeStateStatus::Blocked,
                    MergeableState::Mergeable,
                    true,
                )),
                false, // rollup green: not ci-fixer's
                stale,
            )
        };
        assert_eq!(classify(&base(false)), WatchClass::Healthy);
        assert_eq!(classify(&base(true)), WatchClass::Stuck);
    }

    #[test]
    fn clean_armed_pr_is_healthy() {
        let snap = open(
            Some(merge(
                MergeStateStatus::Clean,
                MergeableState::Mergeable,
                true,
            )),
            false,
            true,
        );
        assert_eq!(classify(&snap), WatchClass::Healthy);
    }

    #[test]
    fn is_armed_and_armed_since_read_the_marker() {
        let comments = vec![
            PrComment {
                body: "just chatter".into(),
                created_at: "2020-01-01T00:00:00Z".into(),
            },
            PrComment {
                body: crate::engine::auto_merger::armed_marker("abc123"),
                created_at: "2026-07-01T00:00:00Z".into(),
            },
        ];
        assert!(is_armed(&comments));
        assert_eq!(armed_since(&comments), parse_ts("2026-07-01T00:00:00Z"));
        // No marker → not armed, no arm-since.
        let none = vec![PrComment {
            body: "hi".into(),
            created_at: "2026-07-01T00:00:00Z".into(),
        }];
        assert!(!is_armed(&none));
        assert_eq!(armed_since(&none), None);
        // Marker with an unparseable time → armed, but no arm-since (never stale).
        let bad = vec![PrComment {
            body: crate::engine::auto_merger::armed_marker("abc"),
            created_at: "not-a-time".into(),
        }];
        assert!(is_armed(&bad));
        assert_eq!(armed_since(&bad), None);
    }

    #[test]
    fn stuck_comment_names_the_state_and_the_recovery() {
        let snap = open(
            Some(merge(
                MergeStateStatus::Blocked,
                MergeableState::Mergeable,
                true,
            )),
            false,
            true,
        );
        let c = stuck_comment(&snap);
        assert!(c.contains("Blocked"), "{c}");
        assert!(c.contains("meguri:needs-human"), "{c}");
    }
}
