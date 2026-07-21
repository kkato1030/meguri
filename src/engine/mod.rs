pub mod ci_fixer;
pub mod cleaner;
pub mod conflict_resolver;
pub mod decompose_materializer;
pub mod escalation;
pub mod fixer;
pub mod flow;
pub mod issue_reconciler;
pub mod plan_handoff;
pub mod planner;
pub mod pr_reviewer;
pub mod reaper;
pub mod reconcile_body_edits;
pub mod routing_drift;
pub mod schedule;
pub mod scheduler;
pub mod self_review;
pub mod spec_fixer;
pub mod spec_worker;
pub mod triage;
pub mod worker;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::{Config, PlanDelivery, ProjectConfig};
use crate::forge::{self, Forge, PullRequest};
use crate::gitops;
use crate::mux::Multiplexer;
use crate::notify::{Notification, Notifier};
use crate::store::{DesiredState, InteractionState, LANE_AUTHOR, LANE_PR_REVIEW, Store};
use crate::tasks::{TaskKey, TaskSource};
use crate::turn::TurnControl;

/// Everything a loop needs to drive runs for one project.
#[derive(Clone)]
pub struct Deps {
    pub store: Store,
    pub mux: Arc<dyn Multiplexer>,
    /// The GitHub forge — issue reading, PR/label/review operations. `None`
    /// in local mode; the forge-dependent loops (fixer, pr-reviewer,
    /// spec-worker, conflict-resolver, ci-fixer, cleaner) then discover
    /// nothing. Task coordination goes through [`Deps::task_source`], not here.
    pub forge: Option<Arc<dyn Forge>>,
    /// The task coordination layer (discover / claim / release / escalate /
    /// complete) — `LabelTaskSource` in github mode, `LocalTaskSource` in
    /// local mode.
    pub task_source: Arc<dyn TaskSource>,
    /// Shared across every run of the project so the per-run notification
    /// throttle survives turn boundaries.
    pub notifier: Arc<Notifier>,
    /// Builds a forge for a repo slug — how cross-repo decomposition reaches a
    /// workspace sibling's repository (issue #154). Production is
    /// `GhForgeFactory`; tests inject fakes. Only ever consulted for siblings;
    /// the project's own repo uses [`Deps::forge`].
    pub forge_factory: Arc<dyn crate::forge::ForgeFactory>,
    pub config: Config,
    pub project: ProjectConfig,
    /// Per-tick cache of `list_open_prs`, shared by the fixer-family loops
    /// (fixer / ci_fixer / conflict_resolver) so their discovery costs the
    /// forge one call per project per tick instead of three (issue #170).
    /// `Scheduler` clears it at the top of every tick; see [`OpenPrCache`].
    pub open_prs: OpenPrCache,
}

impl Deps {
    /// Assemble github-mode deps: the forge is present and its labels are the
    /// coordination layer, so `task_source` is a [`LabelTaskSource`] wrapping
    /// it. This is the shape `app::build_coordination` produces for github
    /// projects; tests use it so their FakeForge flows through the same
    /// `TaskSource` seam production does (issue #54 acceptance criterion 6).
    /// The notifier is built from the config's `[notifications]` section.
    pub fn with_label_source(
        store: Store,
        mux: Arc<dyn Multiplexer>,
        forge: Arc<dyn Forge>,
        config: Config,
        project: ProjectConfig,
    ) -> Self {
        let task_source = Arc::new(crate::tasks::LabelTaskSource::new(
            forge.clone(),
            store.clone(),
            project.id.clone(),
            config.reconcile,
            project.cadence.clone(),
        ));
        let notifier = Arc::new(Notifier::from_config(&config.notifications));
        Self {
            store,
            mux,
            forge: Some(forge),
            task_source,
            notifier,
            forge_factory: Arc::new(crate::forge::gh::GhForgeFactory),
            config,
            project,
            open_prs: OpenPrCache::default(),
        }
    }

    /// Swap in a custom [`ForgeFactory`] (cross-repo decomposition tests inject
    /// fakes for workspace siblings). Builder-style so the common
    /// `with_label_source` path stays a single call.
    pub fn with_forge_factory(mut self, factory: Arc<dyn crate::forge::ForgeFactory>) -> Self {
        self.forge_factory = factory;
        self
    }

    /// A run-scoped clone whose `project` folds in the run's pinned repo
    /// `meguri.toml` (issue #165). The precedence is
    /// `builtin < host global < repo < host [projects.*] override`: a field the
    /// host project already set wins wholesale; otherwise the repo value fills
    /// it in. Cheap — `Deps` shares its store/mux/forge via `Arc`.
    ///
    /// `[pr]` is the one section with a key-level boundary (ADR 0011): the
    /// host's `[projects.pr]` still wins wholesale (draft *and* auto_merge), but
    /// when the host set no project `[pr]`, the repo's `draft` applies while
    /// `auto_merge` stays host-global — the repo can never arm auto-merge.
    pub fn with_repo_config(&self, repo: &crate::config::RepoConfig) -> Self {
        let mut project = self.project.clone();
        if project.language.is_none() {
            project.language = repo.language.clone();
        }
        if project.check_command.is_none() {
            project.check_command = repo.check_command.clone();
        }
        if project.pr.is_none()
            && let Some(draft) = repo.pr.as_ref().and_then(|p| p.draft)
        {
            project.pr = Some(crate::config::PrConfig {
                draft,
                auto_merge: self.config.pr.auto_merge.clone(),
            });
        }
        let mut deps = self.clone();
        deps.project = project;
        deps
    }

    /// The clone path this project's loops operate on. Resolves through
    /// [`Config::repo_path_for`]: an explicit `repo_path`, or the derived
    /// managed-clone path (`~/.meguri/repos/<id>`) when omitted. The single
    /// accessor every loop uses instead of reading `project.repo_path` directly,
    /// so the derivation lives in one place.
    pub fn repo_path(&self) -> std::path::PathBuf {
        self.config.repo_path_for(&self.project)
    }

    /// The forge for github-mode loops. Panics if absent — only the
    /// forge-dependent loops run without a forge, and they short-circuit their
    /// discovery before ever reaching here.
    pub fn forge(&self) -> &Arc<dyn Forge> {
        self.forge
            .as_ref()
            .expect("forge is required for this loop (github mode)")
    }

    /// Push a notification for each watched label an issue meguri just created
    /// in *this project's* repo (per-project `[projects.notify]`, issue #205).
    /// The shared hook every own-repo `create_issue` site calls right after
    /// creation — scheduler fire, cleaner/triage reports, planner children.
    /// Cross-repo sibling children are excluded: this project's watch does not
    /// govern another repo's issues. Best-effort.
    pub async fn notify_created_issue(&self, number: i64, title: &str, labels: &[&str]) {
        let watched = self
            .project
            .notify
            .as_ref()
            .map(|n| n.labels.as_slice())
            .unwrap_or(&[]);
        self.notifier
            .notify_labels(number, title, watched, labels)
            .await;
    }
}

/// Materialize a project's managed bare clone if it is declared but missing —
/// the level-triggered reconcile step for `repo_path` (ADR 0012 / 0018). Called
/// at the very top of each scheduler tick, before anything (redispatch,
/// discover, sweeps) touches `repo_path`, and before a one-shot `meguri run`
/// prepares its worktree.
///
/// A no-op unless the project is github mode **and** its `repo_path` was omitted
/// (a managed clone): an explicit `repo_path` is the host's own clone and meguri
/// never clones over it; local mode has no remote to clone from.
///
/// On failure it emits `repo.clone.failed` and returns the error, so the caller
/// can exclude the project from this tick and retry next tick. It does NOT raise
/// `needs-human` or notify: at this point there is no run/issue/PR to key those
/// to, and the failure is an operator config/auth/network problem that
/// self-heals once fixed — `doctor` is the human-facing surface (ADR 0018).
pub async fn ensure_project_clone(deps: &Deps) -> Result<()> {
    if deps.project.mode != crate::config::ProjectMode::Github
        || !deps.config.is_managed_clone(&deps.project)
    {
        return Ok(());
    }
    let Some(slug) = deps.project.repo_slug.clone() else {
        return Ok(()); // validate guarantees a github slug; stay defensive
    };
    let dest = deps.repo_path();
    // Emit `repo.cloned` only on the tick that actually materializes it, not on
    // every healthy no-op tick.
    let was_absent = matches!(
        gitops::clone_health(&dest, &slug).await,
        gitops::CloneHealth::Absent
    );
    match gitops::ensure_bare_clone(&dest, &slug).await {
        Ok(()) => {
            if was_absent {
                let _ = deps.store.emit(
                    None,
                    "repo.cloned",
                    serde_json::json!({ "slug": slug, "dest": dest.to_string_lossy() }),
                );
            }
            Ok(())
        }
        Err(e) => {
            // Level-triggered: emitted every failing tick (not deduped) — the
            // observation point for "still not fixed".
            let _ = deps.store.emit(
                None,
                "repo.clone.failed",
                serde_json::json!({
                    "slug": slug,
                    "dest": dest.to_string_lossy(),
                    "reason": format!("{e:#}"),
                }),
            );
            Err(e)
        }
    }
}

/// Per-tick cache of `list_open_prs` (issue #170): lazily filled by whichever
/// fixer-family loop's `discover` runs first for a project this tick, reused
/// by the other two, and reset by `Scheduler::discover` at the top of the
/// next tick. Mirrors `reaper::IssueStates`, adapted to `Deps`'s shape — one
/// `Deps` per project rather than a map keyed by id, so a bare `Mutex<Option<_>>`
/// behind a cheap-to-clone `Arc` is enough.
#[derive(Clone, Default)]
pub struct OpenPrCache(Arc<Mutex<Option<Vec<PullRequest>>>>);

impl OpenPrCache {
    /// The project's open PRs: the cached list if this tick already fetched
    /// one, otherwise one `list_open_prs` call that fills the cache.
    pub async fn get(&self, deps: &Deps) -> Result<Vec<PullRequest>> {
        let mut cached = self.0.lock().await;
        if let Some(prs) = cached.as_ref() {
            return Ok(prs.clone());
        }
        let prs = deps.forge().list_open_prs().await?;
        *cached = Some(prs.clone());
        Ok(prs)
    }

    /// Drop the cached list so the next tick's first caller re-fetches.
    pub async fn clear(&self) {
        *self.0.lock().await = None;
    }
}

/// Head-branch prefix identifying meguri's own PRs — the fixer-family loops
/// (fixer / ci_fixer / conflict_resolver) only ever touch work meguri opened.
pub(crate) const MEGURI_BRANCH_PREFIX: &str = "meguri/";

/// Whether the project uses combined plan delivery (ADR 0008) — the mode in
/// which a `spec-ready` PR's branch belongs to the spec worker, so no
/// fixer-family loop may touch it.
pub(crate) fn is_combined(deps: &Deps) -> bool {
    deps.project.plan_delivery == PlanDelivery::Combined
}

/// Whether a fixer-family loop (fixer / ci_fixer / conflict_resolver) may
/// touch `pr` at all, independent of the loop's own symptom (review threads /
/// red CI / conflicts). The three loops used to carry near-identical copies
/// of this guard, which let them drift apart silently: conflict_resolver's
/// copy never gained the `spec-ready` gate the other two got under ADR 0008
/// (issue #170) — a resolver could merge the base into a branch the spec
/// worker still owned. Lifted here so the shared gates cannot drift again;
/// only each loop's own symptom check stays outside.
///
/// `skip_spec_ready` is the one gate that legitimately varies: pass
/// `is_combined(deps)`. Under combined delivery a `spec-ready` PR's branch
/// belongs to the spec worker's takeover (ADR 0008 §6), so no fixer-family
/// loop may touch it; under separate delivery the spec worker never takes
/// the branch over, so a `spec-ready` spec/ADR PR is a standalone PR like any
/// other.
pub fn pr_is_touchable(pr: &PullRequest, skip_spec_ready: bool) -> Option<String> {
    if pr.state != "open" {
        return Some(format!("PR #{} is {} (not open)", pr.number, pr.state));
    }
    if !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX) {
        return Some(format!(
            "PR #{} head `{}` was not opened by meguri",
            pr.number, pr.head_branch
        ));
    }
    if skip_spec_ready && pr.has_label(forge::LABEL_SPEC_READY) {
        return Some(format!(
            "PR #{} is {} (the spec worker owns the branch)",
            pr.number,
            forge::LABEL_SPEC_READY
        ));
    }
    if pr.has_label(forge::LABEL_HOLD) {
        return Some(format!("PR #{} is on hold", pr.number));
    }
    if pr.has_label(forge::LABEL_WORKING) {
        return Some(format!(
            "PR #{} is already claimed ({})",
            pr.number,
            forge::LABEL_WORKING
        ));
    }
    if pr.has_label(forge::LABEL_NEEDS_HUMAN) {
        return Some(format!(
            "PR #{} is escalated ({})",
            pr.number,
            forge::LABEL_NEEDS_HUMAN
        ));
    }
    None
}

/// A unit of work a loop wants a run for: the task to drive. The `key` is the
/// coordination-layer identity — a github issue number or a local task row
/// (issue #54). PR-targeted loops resolve the canonical issue via
/// [`canonical_key`] and carry the PR number in their checkpoint.
#[derive(Debug, Clone)]
pub struct Target {
    /// The coordination-layer identity of the task (github issue or local
    /// task row). Also the run-creation and dispatch-sort key.
    pub key: TaskKey,
    pub title: String,
    /// The cadence bucket (issue #148) discovery matched, if any. The scheduler
    /// stamps it onto the created run so consumption can be counted. `None`
    /// outside any cadence rule and for all non-task-source loops.
    pub cadence_label: Option<String>,
}

/// The GitHub issue a PR belongs to: the branch encoding first
/// (`meguri/<issue>-…`, always present on meguri's own PRs), then a closing
/// keyword (`Closes #N`) in the PR body — the fallback for human-made heads.
/// None when neither resolves (the caller degrades to the PR number).
pub fn canonical_issue(pr: &PullRequest) -> Option<i64> {
    gitops::issue_from_branch(&pr.head_branch).or_else(|| closes_issue(&pr.body))
}

/// The lifetime key a PR's work is filed under: its canonical issue, or the
/// PR number itself as the degraded fallback. Discovery and prepare-work
/// re-resolution both match on this same expression, so they cannot drift.
pub fn canonical_key(pr: &PullRequest) -> i64 {
    canonical_issue(pr).unwrap_or(pr.number)
}

/// First issue referenced by a GitHub closing keyword (`Closes #N` et al.)
/// in a PR body.
fn closes_issue(body: &str) -> Option<i64> {
    const KEYWORDS: &[&str] = &[
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let lower = body.to_lowercase();
    for (i, _) in lower.match_indices('#') {
        let head = lower[..i].trim_end();
        // Whole-word match only ("encloses #41" must not count).
        let is_keyword = KEYWORDS.iter().any(|k| {
            head.ends_with(k)
                && !head[..head.len() - k.len()]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
        });
        if !is_keyword {
            continue;
        }
        let digits: String = lower[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse() {
            return Some(n);
        }
    }
    None
}

/// The single open PR whose [`canonical_key`] is `issue`, re-resolved at
/// prepare-work time (the run only carries the issue number). None when no
/// open PR matches — or when more than one does, which the caller treats as
/// a benign race and skips.
pub async fn open_pr_for_issue(deps: &Deps, issue: i64) -> Result<Option<PullRequest>> {
    let mut matches: Vec<PullRequest> = deps
        .forge()
        .list_open_prs()
        .await?
        .into_iter()
        .filter(|pr| canonical_key(pr) == issue)
        .collect();
    match matches.len() {
        1 => Ok(Some(matches.remove(0))),
        _ => Ok(None),
    }
}

/// The pane lane a loop's runs live in: the pr-reviewer keeps its independent
/// `pr-review` lane; every other loop shares the issue's `author` lane (the
/// cleaner's report issue is only ever touched by the cleaner, so the default
/// lane cannot collide). The internal self-review turn runs in its own
/// `self-review` lane, but that lane is entered explicitly by the flow, not
/// via a loop_kind, so it is not resolved here.
pub fn lane_for_loop(loop_kind: &str) -> &'static str {
    if loop_kind == pr_reviewer::KIND {
        LANE_PR_REVIEW
    } else {
        LANE_AUTHOR
    }
}

/// Terminal outcomes of driving one run (shared by every loop).
#[derive(Debug)]
pub enum WorkerOutcome {
    Succeeded {
        pr_url: String,
    },
    Stopped,
    Interrupted(String),
    /// Benign race: the issue was held or de-labeled between discovery and
    /// claim (e.g. another run already shipped it). No escalation.
    Skipped(String),
    /// The agent found a design decision must precede implementation; the
    /// issue was handed to the planner (issue #22). A normal ending, not a
    /// failure — the reason (agent's summary) is left as an issue comment.
    NeedsPlan(String),
    /// The planner split the issue into sub-issues instead of writing a spec
    /// (issue #24). The second normal planner ending — the rationale
    /// (agent's summary) is left as an issue comment.
    Decomposed(String),
}

/// A schedulable loop: discovers actionable targets for a project and drives
/// runs to a terminal outcome. `kind()` is persisted in `runs.loop_kind` so
/// the scheduler can route a run back to its loop after a restart.
#[async_trait]
pub trait Loop: Send + Sync {
    /// Stable identifier stored in `runs.loop_kind`.
    fn kind(&self) -> &'static str;

    /// Find targets that need a run for this project. Discovery must be
    /// idempotent: targets already covered by an active run are filtered by
    /// the scheduler via the unique (project, loop, issue) run index.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>>;

    /// Drive one run to a terminal outcome, resuming from its checkpoint.
    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome>;
}

/// The dispatch priority of a `runs.loop_kind` (ADR 0001 → ADR 0012 §5): the
/// smaller the rank, the closer to merge, the earlier it dispatches. This is
/// the explicit form of the old "registration order is priority" — the value is
/// the loop's index in [`default_loops`], now needed because the reconciler
/// (issue #223) enqueues fixer-family runs *outside* the discovery loop, so the
/// scheduler sorts every `queued` run by this key rather than by creation order.
/// An unknown loop_kind sorts last (kept stable, never panics).
pub fn dispatch_rank(loop_kind: &str) -> u8 {
    match loop_kind {
        conflict_resolver::KIND => 0,
        ci_fixer::KIND => 1,
        fixer::KIND => 2,
        spec_fixer::KIND => 3,
        spec_worker::KIND => 4,
        pr_reviewer::KIND => 5,
        worker::KIND => 6,
        planner::KIND => 7,
        cleaner::KIND => 8,
        triage::KIND => 9,
        _ => u8::MAX,
    }
}

/// The loops meguri ships today, in dispatch-priority order (the pipeline
/// reversed = closest to merge first). The scheduler hands out slots from
/// the head of this list, so ordering alone is the priority mechanism.
pub fn default_loops() -> Vec<Arc<dyn Loop>> {
    vec![
        Arc::new(conflict_resolver::ConflictResolverLoop),
        Arc::new(ci_fixer::CiFixerLoop),
        Arc::new(fixer::FixerLoop),
        // Plan-side fixer family (issue #188): unparks spec PRs the plan
        // review flagged, so it sits with the other fixers, above new-work
        // loops.
        Arc::new(spec_fixer::SpecFixerLoop),
        Arc::new(spec_worker::SpecWorkerLoop),
        Arc::new(pr_reviewer::PrReviewerLoop),
        Arc::new(worker::WorkerLoop),
        Arc::new(planner::PlannerLoop),
        Arc::new(cleaner::CleanerLoop),
        Arc::new(triage::TriageLoop),
    ]
}

/// TurnControl over the sqlite store: the CLI writes `desired_state`,
/// live turns converge to it and report state/events back. Additionally
/// pages a human (via the throttled notifier) on `turn.awaiting_human`.
pub struct StoreControl {
    pub store: Store,
    pub run_id: String,
    pub notifier: Arc<Notifier>,
}

#[async_trait]
impl TurnControl for StoreControl {
    async fn desired(&self) -> Option<DesiredState> {
        self.store.read_desired_state(&self.run_id).ok().flatten()
    }

    async fn set_interaction(&self, state: InteractionState) {
        let _ = self
            .store
            .update_interaction_state(&self.run_id, Some(state));
    }

    async fn event(&self, kind: &str, data: serde_json::Value) {
        let awaiting = (kind == "turn.awaiting_human").then(|| {
            let run = self.store.get_run(&self.run_id).ok().flatten();
            Notification::awaiting_human(
                self.run_id.clone(),
                run.as_ref().map_or(0, |r| r.issue_number),
                run.and_then(|r| r.issue_title),
                data["reason"].as_str().unwrap_or("unknown"),
                // Turn-scoped escalations point at the live pane, never a URL.
                Some(data["attach"].as_str().unwrap_or_default().to_string()),
                None,
            )
        });
        let _ = self.store.emit(Some(&self.run_id), kind, data);
        if let Some(n) = awaiting {
            self.notifier.notify(&n).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: i64, head_branch: &str, body: &str) -> PullRequest {
        PullRequest {
            number,
            title: "t".into(),
            body: body.into(),
            url: format!("https://fake.example/pr/{number}"),
            head_branch: head_branch.into(),
            head_sha: String::new(),
            state: "open".into(),
            is_draft: false,
            labels: vec![],
        }
    }

    #[test]
    fn canonical_issue_prefers_branch_then_closing_keyword() {
        // Branch encoding wins even over a closing keyword.
        let p = pr(12, "meguri/5-add-caching-abc123", "Closes #9.");
        assert_eq!(canonical_issue(&p), Some(5));
        assert_eq!(canonical_key(&p), 5);

        // Human-made head: the body's closing keyword resolves it.
        let p = pr(12, "feature/manual", "Some spec.\n\ncloses #7");
        assert_eq!(canonical_issue(&p), Some(7));
        assert_eq!(canonical_key(&p), 7);

        // Degraded mode: neither resolves — the PR number is the key.
        let p = pr(12, "feature/manual", "no reference here");
        assert_eq!(canonical_issue(&p), None);
        assert_eq!(canonical_key(&p), 12);
    }

    #[test]
    fn closing_keywords_parse_like_github() {
        for body in [
            "Closes #41",
            "Closes#41",
            "fixes #41 and more",
            "Resolved #41",
            "prefix text\nclose #41",
        ] {
            assert_eq!(canonical_issue(&pr(1, "x", body)), Some(41), "body: {body}");
        }
        for body in ["see #41", "closes GH-41 only", "#41 alone", "encloses #41"] {
            assert_eq!(canonical_issue(&pr(1, "x", body)), None, "body: {body}");
        }
    }

    #[test]
    fn lane_is_pr_review_only_for_the_pr_reviewer() {
        assert_eq!(lane_for_loop(pr_reviewer::KIND), LANE_PR_REVIEW);
        for kind in [
            "worker",
            "planner",
            "spec-worker",
            "spec-fixer",
            "fixer",
            "ci-fixer",
            "conflict-resolver",
            "cleaner",
            "triage",
        ] {
            assert_eq!(lane_for_loop(kind), LANE_AUTHOR, "loop: {kind}");
        }
    }

    #[test]
    fn spec_fixer_sits_in_the_fixer_family_above_new_work() {
        // Registration order is priority (issue #188): the spec-fixer must
        // unpark a spec PR before the worker/planner start new work, and it
        // belongs after the fixer, with the guard/worker/planner behind it.
        let kinds: Vec<&str> = default_loops().iter().map(|l| l.kind()).collect();
        let pos = |k: &str| kinds.iter().position(|x| *x == k).expect(k);
        assert!(pos("fixer") < pos("spec-fixer"));
        assert!(pos("spec-fixer") < pos("spec-worker"));
        assert!(pos("spec-fixer") < pos("worker"));
        assert!(pos("spec-fixer") < pos("planner"));
    }

    #[test]
    fn touchable_guards_state_ownership_and_claim_labels() {
        let base = pr(3, "meguri/9-add-feature-abc123", "");
        assert!(pr_is_touchable(&base, true).is_none());

        let merged = PullRequest {
            state: "merged".into(),
            ..base.clone()
        };
        assert!(pr_is_touchable(&merged, true).unwrap().contains("merged"));

        let human = PullRequest {
            head_branch: "feature/manual".into(),
            ..base.clone()
        };
        assert!(
            pr_is_touchable(&human, true)
                .unwrap()
                .contains("not opened by meguri")
        );

        // spec-ready: skipped only under combined delivery (the spec worker
        // owns the branch); an ordinary standalone PR under separate (ADR
        // 0008 §6, issue #170).
        let spec_ready = PullRequest {
            labels: vec![forge::LABEL_SPEC_READY.to_string()],
            ..base.clone()
        };
        assert!(
            pr_is_touchable(&spec_ready, true)
                .unwrap()
                .contains(forge::LABEL_SPEC_READY)
        );
        assert!(
            pr_is_touchable(&spec_ready, false).is_none(),
            "separate delivery: a spec-ready PR is touchable"
        );

        let held = PullRequest {
            labels: vec![forge::LABEL_HOLD.to_string()],
            ..base.clone()
        };
        assert!(pr_is_touchable(&held, true).unwrap().contains("hold"));

        let working = PullRequest {
            labels: vec![forge::LABEL_WORKING.to_string()],
            ..base.clone()
        };
        assert!(
            pr_is_touchable(&working, true)
                .unwrap()
                .contains(forge::LABEL_WORKING)
        );

        let needs_human = PullRequest {
            labels: vec![forge::LABEL_NEEDS_HUMAN.to_string()],
            ..base
        };
        assert!(
            pr_is_touchable(&needs_human, true)
                .unwrap()
                .contains(forge::LABEL_NEEDS_HUMAN)
        );
    }

    #[tokio::test]
    async fn open_pr_cache_fetches_once_per_project_per_tick() {
        use crate::mux::fake::FakeMux;

        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        forge.push_pr("meguri/9-add-feature-abc123", "Add feature (#9)", &[]);
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
            Arc::new(FakeMux::new(false)),
            forge.clone(),
            Config::default(),
            project,
        );

        let first = deps.open_prs.get(&deps).await.unwrap();
        assert_eq!(first.len(), 1);
        // A second PR appears on the forge mid-tick: the cache must not see
        // it until cleared, proving the three fixer-family loops would share
        // one fetch this tick.
        forge.push_pr("meguri/10-other-def456", "Other (#10)", &[]);
        let second = deps.open_prs.get(&deps).await.unwrap();
        assert_eq!(second.len(), 1, "cached list reused within the tick");

        deps.open_prs.clear().await;
        let third = deps.open_prs.get(&deps).await.unwrap();
        assert_eq!(third.len(), 2, "next tick re-fetches");
    }
}
