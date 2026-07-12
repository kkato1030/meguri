//! The task coordination layer, split out of [`Forge`](crate::forge::Forge):
//! discover / claim / release / escalate / complete. This is the seam that
//! GitHub labels used to fill; swapping it is how meguri runs against a repo
//! whose labels it cannot (or must not) touch, and — later — how a remote DB
//! coordinates several hosts (see ADR 0003).
//!
//! Two implementations ship today:
//! - [`LabelTaskSource`]: the current label-driven behavior, wrapping a
//!   [`Forge`]. `meguri:ready`/`meguri:plan` are the queue, `meguri:working`
//!   the claim, `meguri:needs-human` the escalation. task identity is the
//!   issue number ([`TaskKey::Issue`]); no DB row mirrors the labels.
//! - [`LocalTaskSource`]: a sqlite `tasks` table. task identity is the row id
//!   ([`TaskKey::Local`]); state lives entirely in the local store.
//!
//! The `claim` contract is deliberately async + single-atomic-operation so
//! the Phase 4 remote-DB implementation is one more `impl TaskSource` rather
//! than a reshaped contract: `claim(key, host) -> Option<Task>`, where `None`
//! is a benign race (someone else took it, or it is no longer actionable).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::engine::{planner, worker};
use crate::forge::{self, Forge};
use crate::store::Store;

/// The claiming host id stored in `tasks.claimed_by`. Fixed on a single
/// machine (Phase 1–3); Phase 4's remote DB gives each host its own.
pub const LOCAL_HOST: &str = "local";

/// What a task is queued for. Maps to the worker (`work`) and planner
/// (`plan`) loops, and to the `meguri:ready`/`meguri:plan` labels in
/// [`LabelTaskSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Work,
    Plan,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Plan => "plan",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "work" => Some(Self::Work),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    /// The `(trigger label, loop kind)` this task kind maps to in label mode.
    /// The loop kind scopes the succeeded-run discovery guard exactly as the
    /// old `discover_by_label` call sites did.
    fn label_and_loop(self) -> (&'static str, &'static str) {
        match self {
            Self::Work => (forge::LABEL_READY, worker::KIND),
            Self::Plan => (forge::LABEL_PLAN, planner::KIND),
        }
    }
}

/// A task's identity across the coordination layer. github mode keeps the
/// issue number as the key (labels remain the only source of truth — no
/// mirror row); local/silent tasks use the `tasks` row id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskKey {
    Issue(i64),
    Local(i64),
}

impl TaskKey {
    /// The numeric id inside the key — the github issue number or the local
    /// task row id — regardless of variant. For display and test assertions;
    /// callers that must distinguish the origin match on the variant instead.
    pub fn number(&self) -> i64 {
        match self {
            TaskKey::Issue(n) | TaskKey::Local(n) => *n,
        }
    }
}

/// An actionable task as the coordination layer hands it to a loop.
#[derive(Debug, Clone)]
pub struct Task {
    pub key: TaskKey,
    pub kind: TaskKind,
    pub title: String,
    pub body: String,
    /// The GitHub issue a local task points at (`origin = github:<N>`), for
    /// silent mode. `None` for plain local tasks.
    pub issue: Option<i64>,
    /// The task opted into auto-merge (auto-merge 1/3, #41): github mode reads
    /// the `meguri:automerge` label; local mode is always `false`. Carried into
    /// the checkpoint at claim time and applied when the PR opens.
    pub automerge: bool,
}

/// The task coordination layer: discover / claim / release / escalate /
/// complete. `claim` is a single atomic operation (see the module docs).
#[async_trait]
pub trait TaskSource: Send + Sync {
    /// Actionable tasks of `kind` (queued *or* needs_human, so a re-claim can
    /// clear an escalation — the label version does the same by leaving the
    /// trigger label on an escalated issue). Idempotent.
    async fn discover(&self, kind: TaskKind) -> Result<Vec<Task>>;

    /// Claim a task as one atomic operation. `None` is a benign race (someone
    /// else took it, or it is no longer actionable) and ends the run Skipped.
    async fn claim(&self, key: &TaskKey, host: &str) -> Result<Option<Task>>;

    /// Release a claim (`meguri stop`, needs-plan demotion).
    async fn release(&self, key: &TaskKey) -> Result<()>;

    /// Hand the task to a human; `reason` is stored durably (label + comment
    /// in github mode, `status='needs_human'` + `reason` in local mode).
    async fn escalate(&self, key: &TaskKey, reason: &str) -> Result<()>;

    /// The task shipped a deliverable (github: drop the trigger/working
    /// labels; local: `status='done'`).
    async fn complete(&self, key: &TaskKey) -> Result<()>;
}

/// The needs-human comment left on an escalated issue (shared by
/// [`LabelTaskSource`] and [`crate::engine::flow::escalate_on_forge`]).
pub fn needs_human_comment(reason: &str) -> String {
    format!(
        "🔁 **meguri** could not finish this issue and needs a human.\n\n> {reason}\n\n\
         The agent's pane (if still open) has the full context — \
         see `meguri ps` / `meguri attach` on the host running meguri."
    )
}

/// Dependency gate (looper ADR-0004): only a blocker closed as completed
/// resolves; open blockers, not_planned/duplicate closes, and blockers we
/// cannot read all keep the issue out of discovery.
async fn has_unresolved_blockers(forge: &dyn Forge, issue: i64) -> bool {
    match forge.blocked_by(issue).await {
        Ok(blockers) => blockers.iter().any(|b| !b.resolved()),
        Err(_) => true,
    }
}

/// The current label-driven coordination layer, wrapping a [`Forge`]. This is
/// the verbatim behavior of the old `flow::discover_by_label` / `claim_issue`
/// / `escalate_on_forge` / worker `settle_labels`, now behind the trait so a
/// non-label implementation can stand in.
pub struct LabelTaskSource {
    forge: Arc<dyn Forge>,
    store: Store,
    project_id: String,
}

impl LabelTaskSource {
    pub fn new(forge: Arc<dyn Forge>, store: Store, project_id: String) -> Self {
        Self {
            forge,
            store,
            project_id,
        }
    }
}

#[async_trait]
impl TaskSource for LabelTaskSource {
    async fn discover(&self, kind: TaskKind) -> Result<Vec<Task>> {
        let (label, loop_kind) = kind.label_and_loop();
        let issues = self.forge.list_issues_with_label(label).await?;
        let mut tasks = Vec::new();
        for issue in issues {
            if issue.has_label(forge::LABEL_HOLD) || issue.has_label(forge::LABEL_WORKING) {
                continue;
            }
            if self
                .store
                .issue_has_succeeded_run(&self.project_id, loop_kind, issue.number)?
            {
                continue;
            }
            if has_unresolved_blockers(&*self.forge, issue.number).await {
                continue;
            }
            tasks.push(Task {
                key: TaskKey::Issue(issue.number),
                kind,
                automerge: issue.has_label(forge::LABEL_AUTOMERGE),
                title: issue.title,
                body: issue.body,
                issue: Some(issue.number),
            });
        }
        Ok(tasks)
    }

    async fn claim(&self, key: &TaskKey, _host: &str) -> Result<Option<Task>> {
        let TaskKey::Issue(n) = *key else {
            return Ok(None); // a local key can never belong to the label source
        };
        let issue = self.forge.get_issue(n).await?;
        // A hold or a missing trigger label is a benign race (the issue
        // changed between discovery and claim — e.g. another run shipped it
        // and removed the trigger label). Re-verify before taking the claim.
        if issue.has_label(forge::LABEL_HOLD) {
            return Ok(None);
        }
        if !(issue.has_label(forge::LABEL_READY) || issue.has_label(forge::LABEL_PLAN)) {
            return Ok(None);
        }
        self.forge.add_label(n, forge::LABEL_WORKING).await?;
        // A fresh claim supersedes a previous run's escalation: the human is
        // no longer needed while this run is in flight (a new failure re-adds
        // the label). Best-effort, like the escalation side.
        let _ = self.forge.remove_label(n, forge::LABEL_NEEDS_HUMAN).await;
        let kind = if issue.has_label(forge::LABEL_PLAN) {
            TaskKind::Plan
        } else {
            TaskKind::Work
        };
        Ok(Some(Task {
            key: *key,
            kind,
            automerge: issue.has_label(forge::LABEL_AUTOMERGE),
            title: issue.title,
            body: issue.body,
            issue: Some(n),
        }))
    }

    async fn release(&self, key: &TaskKey) -> Result<()> {
        if let TaskKey::Issue(n) = *key {
            let _ = self.forge.remove_label(n, forge::LABEL_WORKING).await;
        }
        Ok(())
    }

    async fn escalate(&self, key: &TaskKey, reason: &str) -> Result<()> {
        if let TaskKey::Issue(n) = *key {
            let _ = self.forge.add_label(n, forge::LABEL_NEEDS_HUMAN).await;
            let _ = self.forge.remove_label(n, forge::LABEL_WORKING).await;
            let _ = self.forge.comment(n, &needs_human_comment(reason)).await;
        }
        Ok(())
    }

    async fn complete(&self, key: &TaskKey) -> Result<()> {
        if let TaskKey::Issue(n) = *key {
            let _ = self.forge.remove_label(n, forge::LABEL_WORKING).await;
            let _ = self.forge.remove_label(n, forge::LABEL_READY).await;
        }
        Ok(())
    }
}

/// The local sqlite `tasks` coordination layer. task identity is the row id;
/// all state (queue, claim, escalation, completion) lives in the store.
pub struct LocalTaskSource {
    store: Store,
    project_id: String,
}

impl LocalTaskSource {
    pub fn new(store: Store, project_id: String) -> Self {
        Self { store, project_id }
    }
}

/// The issue number a local task points at, parsed from its `origin`
/// (`github:<N>` for silent-mode tasks; plain `local` otherwise).
fn origin_issue(origin: &str) -> Option<i64> {
    origin.strip_prefix("github:")?.parse().ok()
}

fn row_to_task(row: crate::store::TaskRow) -> Task {
    Task {
        key: TaskKey::Local(row.id),
        kind: TaskKind::parse(&row.kind).unwrap_or(TaskKind::Work),
        // Local tasks never opt into auto-merge (it is a github-PR concern).
        automerge: false,
        title: row.title,
        body: row.body,
        issue: origin_issue(&row.origin),
    }
}

#[async_trait]
impl TaskSource for LocalTaskSource {
    async fn discover(&self, kind: TaskKind) -> Result<Vec<Task>> {
        Ok(self
            .store
            .discover_tasks(&self.project_id, kind.as_str())?
            .into_iter()
            .map(row_to_task)
            .collect())
    }

    async fn claim(&self, key: &TaskKey, host: &str) -> Result<Option<Task>> {
        let TaskKey::Local(id) = *key else {
            return Ok(None);
        };
        Ok(self
            .store
            .claim_task(id, &self.project_id, host)?
            .map(row_to_task))
    }

    async fn release(&self, key: &TaskKey) -> Result<()> {
        if let TaskKey::Local(id) = *key {
            self.store.release_task(id)?;
        }
        Ok(())
    }

    async fn escalate(&self, key: &TaskKey, reason: &str) -> Result<()> {
        if let TaskKey::Local(id) = *key {
            self.store.escalate_task(id, reason)?;
        }
        Ok(())
    }

    async fn complete(&self, key: &TaskKey) -> Result<()> {
        if let TaskKey::Local(id) = *key {
            self.store.complete_task(id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_kind_maps_to_the_matching_label_and_loop() {
        assert_eq!(
            TaskKind::Work.label_and_loop(),
            (forge::LABEL_READY, worker::KIND)
        );
        assert_eq!(
            TaskKind::Plan.label_and_loop(),
            (forge::LABEL_PLAN, planner::KIND)
        );
    }

    #[test]
    fn task_key_orders_issue_before_local_then_by_id() {
        let mut keys = vec![
            TaskKey::Local(2),
            TaskKey::Issue(9),
            TaskKey::Local(1),
            TaskKey::Issue(3),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                TaskKey::Issue(3),
                TaskKey::Issue(9),
                TaskKey::Local(1),
                TaskKey::Local(2),
            ]
        );
    }

    #[test]
    fn origin_issue_parses_only_github_origins() {
        assert_eq!(origin_issue("github:42"), Some(42));
        assert_eq!(origin_issue("local"), None);
        assert_eq!(origin_issue("github:x"), None);
    }

    /// LabelTaskSource wraps a forge and maps the trait onto labels: discover
    /// keys off the trigger label, claim adds `working` (and clears a stale
    /// `needs-human`), escalate/complete drive the escalation/settle labels.
    #[tokio::test]
    async fn label_source_maps_the_contract_onto_labels() {
        use crate::forge::fake::FakeForge;
        use crate::forge::{LABEL_NEEDS_HUMAN, LABEL_READY, LABEL_WORKING};

        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let src = LabelTaskSource::new(
            forge.clone(),
            Store::open_in_memory().unwrap(),
            "proj".into(),
        );
        let key = TaskKey::Issue(7);

        let tasks = src.discover(TaskKind::Work).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].key, key);

        assert!(src.claim(&key, LOCAL_HOST).await.unwrap().is_some());
        assert!(forge.labels_of(7).contains(&LABEL_WORKING.to_string()));

        src.escalate(&key, "stuck").await.unwrap();
        let labels = forge.labels_of(7);
        assert!(labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
        assert!(!labels.contains(&LABEL_WORKING.to_string()));
        assert_eq!(forge.comments_of(7).len(), 1);

        src.complete(&key).await.unwrap();
        let labels = forge.labels_of(7);
        assert!(!labels.contains(&LABEL_READY.to_string()));
        assert!(!labels.contains(&LABEL_WORKING.to_string()));

        // A local key can never belong to the label source.
        assert!(
            src.claim(&TaskKey::Local(1), LOCAL_HOST)
                .await
                .unwrap()
                .is_none()
        );
    }
}
