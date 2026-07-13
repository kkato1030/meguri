pub mod auto_merger;
pub mod ci_fixer;
pub mod cleaner;
pub mod conflict_resolver;
pub mod fixer;
pub mod flow;
pub mod guard;
pub mod handoff;
pub mod impl_reviewer;
pub mod merge_watch;
pub mod planner;
pub mod reaper;
pub mod scheduler;
pub mod spec_worker;
pub mod worker;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{Config, ProjectConfig};
use crate::forge::{Forge, PullRequest};
use crate::gitops;
use crate::mux::Multiplexer;
use crate::notify::{Notification, Notifier};
use crate::store::{DesiredState, InteractionState, ROLE_AUTHOR, ROLE_REVIEW, Store};
use crate::tasks::{TaskKey, TaskSource};
use crate::turn::TurnControl;

/// Everything a loop needs to drive runs for one project.
#[derive(Clone)]
pub struct Deps {
    pub store: Store,
    pub mux: Arc<dyn Multiplexer>,
    /// The GitHub forge — issue reading, PR/label/review operations. `None`
    /// in local mode; the forge-dependent loops (fixer, reviewer, spec-worker,
    /// conflict-resolver, ci-fixer, impl-reviewer, cleaner) then discover
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
        }
    }

    /// Swap in a custom [`ForgeFactory`] (cross-repo decomposition tests inject
    /// fakes for workspace siblings). Builder-style so the common
    /// `with_label_source` path stays a single call.
    pub fn with_forge_factory(mut self, factory: Arc<dyn crate::forge::ForgeFactory>) -> Self {
        self.forge_factory = factory;
        self
    }

    /// The forge for github-mode loops. Panics if absent — only the
    /// forge-dependent loops run without a forge, and they short-circuit their
    /// discovery before ever reaching here.
    pub fn forge(&self) -> &Arc<dyn Forge> {
        self.forge
            .as_ref()
            .expect("forge is required for this loop (github mode)")
    }
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

/// The pane lane a loop's runs live in: the guard keeps its independent
/// `review` lane; every other loop shares the issue's `author` lane (the
/// cleaner's report issue is only ever touched by the cleaner, so the default
/// lane cannot collide). The internal self-review turn runs in its own
/// `impl-review` lane, but that lane is entered explicitly by the flow, not
/// via a loop_kind, so it is not resolved here.
pub fn role_for_loop(loop_kind: &str) -> &'static str {
    if loop_kind == guard::KIND {
        ROLE_REVIEW
    } else {
        ROLE_AUTHOR
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

/// The loops meguri ships today, in dispatch-priority order (the pipeline
/// reversed = closest to merge first). The scheduler hands out slots from
/// the head of this list, so ordering alone is the priority mechanism.
pub fn default_loops() -> Vec<Arc<dyn Loop>> {
    vec![
        Arc::new(conflict_resolver::ConflictResolverLoop),
        Arc::new(ci_fixer::CiFixerLoop),
        Arc::new(fixer::FixerLoop),
        Arc::new(spec_worker::SpecWorkerLoop),
        Arc::new(guard::GuardLoop),
        Arc::new(worker::WorkerLoop),
        Arc::new(planner::PlannerLoop),
        Arc::new(cleaner::CleanerLoop),
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
            Notification {
                run_id: self.run_id.clone(),
                issue_number: run.as_ref().map_or(0, |r| r.issue_number),
                issue_title: run.and_then(|r| r.issue_title),
                reason: data["reason"].as_str().unwrap_or("unknown").to_string(),
                attach: data["attach"].as_str().unwrap_or_default().to_string(),
            }
        });
        let _ = self.store.emit(Some(&self.run_id), kind, data);
        if let Some(n) = awaiting {
            self.notifier.notify_awaiting_human(&n).await;
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
    fn lane_is_review_only_for_the_guard() {
        assert_eq!(role_for_loop(guard::KIND), ROLE_REVIEW);
        for kind in [
            "worker",
            "planner",
            "spec-worker",
            "fixer",
            "ci-fixer",
            "conflict-resolver",
            "cleaner",
        ] {
            assert_eq!(role_for_loop(kind), ROLE_AUTHOR, "loop: {kind}");
        }
    }
}
