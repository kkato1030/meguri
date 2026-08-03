//! The task coordination layer, split out of [`Forge`](crate::forge::Forge):
//! claim / release / escalate / complete. **The sqlite `tasks` table is the
//! authority for the task lifecycle in every mode** (kernel-pruning-plan
//! Phase 5, 権威反転): GitHub is a projection — labels and comments are
//! written best-effort for the human's benefit, and read only as low-frequency
//! edge signals (the intake sweep, and the claim-time re-verification).
//!
//! Two implementations ship:
//! - [`GithubTaskSource`]: sqlite authority + GitHub projection. Task rows are
//!   imported from `meguri:ready` issues by the intake sweep (or lazily at
//!   claim time), carry `origin = github:<N>`, and keep the issue number as
//!   their engine-facing identity ([`TaskKey::Issue`]) so runs, panes,
//!   worktrees, and PRs stay issue-keyed.
//! - [`LocalTaskSource`]: pure sqlite, no forge. task identity is the row id
//!   ([`TaskKey::Local`]).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::forge::{self, Forge};
use crate::store::Store;

/// The claiming host id stored in `tasks.claimed_by`. Fixed on a single
/// machine; a remote DB would give each host its own.
pub const LOCAL_HOST: &str = "local";

/// What a task is queued for. Only worker work remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Work,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Work => "work",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "work" => Some(Self::Work),
            _ => None,
        }
    }
}

/// A task's identity across the coordination layer. A github-origin task keeps
/// its issue number as the key (runs/panes/worktrees/PRs stay issue-keyed);
/// plain local tasks use the `tasks` row id.
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
    /// The GitHub issue this task points at (`origin = github:<N>`). `None`
    /// for plain local tasks.
    pub issue: Option<i64>,
}

/// The task coordination layer: claim / release / escalate / complete.
/// `claim` is a single atomic operation (an UPDATE guarded on the claimable
/// statuses); `None` is a benign race (someone else took it, or it is no
/// longer actionable) and ends the run Skipped.
#[async_trait]
pub trait TaskSource: Send + Sync {
    async fn claim(&self, key: &TaskKey, host: &str) -> Result<Option<Task>>;

    /// Release a claim (`meguri stop`).
    async fn release(&self, key: &TaskKey) -> Result<()>;

    /// Hand the task to a human; `reason` is stored durably
    /// (`status='needs_human'` + `reason`, plus the label + comment
    /// projection in github mode). `hint` is the launch-mode-aware "how to
    /// look at this" sentence (issue #169) folded into the github comment.
    async fn escalate(&self, key: &TaskKey, reason: &str, hint: &str) -> Result<()>;

    /// The task shipped a deliverable (`status='done'`; github mode also
    /// drops the trigger/working labels as projection).
    async fn complete(&self, key: &TaskKey) -> Result<()>;
}

/// Fallback attach hint for callers with no launch-mode context of their own
/// (pane is always a plausible guess before mode is known).
pub const DEFAULT_ATTACH_HINT: &str = "The agent's pane (if still open) has the full context — \
     see `meguri ps` / `meguri attach` on the host running meguri.";

/// The needs-human comment left on an escalated issue.
pub fn needs_human_comment(reason: &str, hint: &str) -> String {
    format!("🔁 **meguri** could not finish this issue and needs a human.\n\n> {reason}\n\n{hint}")
}

/// The `tasks.origin` value for a task imported from a GitHub issue.
pub fn github_origin(issue: i64) -> String {
    format!("github:{issue}")
}

/// The issue number a task points at, parsed from its `origin`
/// (`github:<N>`; plain `local` otherwise).
pub fn origin_issue(origin: &str) -> Option<i64> {
    origin.strip_prefix("github:")?.parse().ok()
}

/// Dependency gate (looper ADR-0004): only a blocker closed as completed
/// resolves; open blockers, not_planned/duplicate closes, and blockers we
/// cannot read all keep the issue out of dispatch.
pub async fn has_unresolved_blockers(forge: &dyn Forge, issue: i64) -> bool {
    match forge.blocked_by(issue).await {
        Ok(blockers) => blockers.iter().any(|b| !b.resolved()),
        Err(_) => true,
    }
}

/// sqlite-authority coordination for a github project, with the forge as a
/// best-effort projection (権威反転): the row decides, the labels display.
pub struct GithubTaskSource {
    forge: Arc<dyn Forge>,
    store: Store,
    project_id: String,
}

impl GithubTaskSource {
    pub fn new(forge: Arc<dyn Forge>, store: Store, project_id: String) -> Self {
        Self {
            forge,
            store,
            project_id,
        }
    }

    /// The task row an issue key resolves to, if any.
    fn row_of(&self, issue: i64) -> Result<Option<crate::store::TaskRow>> {
        self.store
            .task_by_origin(&self.project_id, &github_origin(issue))
    }
}

#[async_trait]
impl TaskSource for GithubTaskSource {
    /// Claim = "import if absent + claim", idempotent. The forge read is a
    /// re-verification edge signal (the issue may have been held or
    /// de-labeled since import); the sqlite UPDATE is the atomic authority.
    async fn claim(&self, key: &TaskKey, host: &str) -> Result<Option<Task>> {
        let TaskKey::Issue(n) = *key else {
            // A plain local row inside a github project (e.g. seeded by hand).
            let TaskKey::Local(id) = *key else {
                return Ok(None);
            };
            return Ok(self
                .store
                .claim_task(id, &self.project_id, host)?
                .map(row_to_task));
        };
        // Re-verify against the forge: hold or a missing trigger label is a
        // benign race (the issue changed since import / discovery).
        let issue = self.forge.get_issue(n).await?;
        if issue.has_label(forge::LABEL_HOLD) {
            return Ok(None);
        }
        if !issue.has_label(forge::LABEL_READY) {
            return Ok(None);
        }
        // Import lazily when no live row exists (manual `meguri run --issue`
        // before the intake ever saw the issue; a done row means a re-run —
        // a fresh row keeps history).
        let row = match self.row_of(n)? {
            Some(r) if r.status == "held" => return Ok(None),
            Some(r) if matches!(r.status.as_str(), "queued" | "needs_human" | "claimed") => r,
            _ => self.store.create_task(
                &self.project_id,
                TaskKind::Work.as_str(),
                &issue.title,
                &issue.body,
                &github_origin(n),
            )?,
        };
        if self
            .store
            .claim_task(row.id, &self.project_id, host)?
            .is_none()
        {
            return Ok(None); // someone else holds the claim
        }
        // Projection: the working label for the human's dashboard, and a fresh
        // claim supersedes a previous run's escalation. Best-effort.
        let _ = self.forge.add_label(n, forge::LABEL_WORKING).await;
        let _ = self.forge.remove_label(n, forge::LABEL_NEEDS_HUMAN).await;
        Ok(Some(Task {
            key: *key,
            kind: TaskKind::Work,
            title: issue.title,
            body: issue.body,
            issue: Some(n),
        }))
    }

    async fn release(&self, key: &TaskKey) -> Result<()> {
        match *key {
            TaskKey::Issue(n) => {
                if let Some(row) = self.row_of(n)? {
                    self.store.release_task(row.id)?;
                }
                let _ = self.forge.remove_label(n, forge::LABEL_WORKING).await;
            }
            TaskKey::Local(id) => self.store.release_task(id)?,
        }
        Ok(())
    }

    async fn escalate(&self, key: &TaskKey, reason: &str, hint: &str) -> Result<()> {
        match *key {
            TaskKey::Issue(n) => {
                if let Some(row) = self.row_of(n)? {
                    self.store.escalate_task(row.id, reason)?;
                }
                let _ = self.forge.add_label(n, forge::LABEL_NEEDS_HUMAN).await;
                let _ = self.forge.remove_label(n, forge::LABEL_WORKING).await;
                let _ = self
                    .forge
                    .comment(n, &needs_human_comment(reason, hint))
                    .await;
            }
            TaskKey::Local(id) => self.store.escalate_task(id, reason)?,
        }
        Ok(())
    }

    async fn complete(&self, key: &TaskKey) -> Result<()> {
        match *key {
            TaskKey::Issue(n) => {
                if let Some(row) = self.row_of(n)? {
                    self.store.complete_task(row.id)?;
                }
                let _ = self.forge.remove_label(n, forge::LABEL_WORKING).await;
                let _ = self.forge.remove_label(n, forge::LABEL_READY).await;
            }
            TaskKey::Local(id) => self.store.complete_task(id)?,
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

pub(crate) fn row_to_task(row: crate::store::TaskRow) -> Task {
    Task {
        key: TaskKey::Local(row.id),
        kind: TaskKind::parse(&row.kind).unwrap_or(TaskKind::Work),
        title: row.title,
        body: row.body,
        issue: origin_issue(&row.origin),
    }
}

#[async_trait]
impl TaskSource for LocalTaskSource {
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

    async fn escalate(&self, key: &TaskKey, reason: &str, _hint: &str) -> Result<()> {
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

    /// GithubTaskSource: the sqlite row is the authority, the labels are the
    /// projection. Claim imports lazily, marks working; escalate/complete
    /// settle both sides.
    #[tokio::test]
    async fn github_source_claims_via_row_and_projects_labels() {
        use crate::forge::fake::FakeForge;
        use crate::forge::{LABEL_NEEDS_HUMAN, LABEL_READY, LABEL_WORKING};

        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let store = Store::open_in_memory().unwrap();
        let src = GithubTaskSource::new(forge.clone(), store.clone(), "proj".into());
        let key = TaskKey::Issue(7);

        // Claim imports the row lazily and claims it atomically.
        assert!(src.claim(&key, LOCAL_HOST).await.unwrap().is_some());
        let row = store.task_by_origin("proj", "github:7").unwrap().unwrap();
        assert_eq!(row.status, "claimed");
        assert!(forge.labels_of(7).contains(&LABEL_WORKING.to_string()));

        // A second claim is a benign race.
        assert!(src.claim(&key, LOCAL_HOST).await.unwrap().is_none());

        src.escalate(&key, "stuck", DEFAULT_ATTACH_HINT)
            .await
            .unwrap();
        let row = store.task_by_origin("proj", "github:7").unwrap().unwrap();
        assert_eq!(row.status, "needs_human");
        let labels = forge.labels_of(7);
        assert!(labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
        assert!(!labels.contains(&LABEL_WORKING.to_string()));
        assert_eq!(forge.comments_of(7).len(), 1);

        // Re-claim un-escalates (needs_human is claimable).
        assert!(src.claim(&key, LOCAL_HOST).await.unwrap().is_some());
        src.complete(&key).await.unwrap();
        let row = store.task_by_origin("proj", "github:7").unwrap().unwrap();
        assert_eq!(row.status, "done");
        let labels = forge.labels_of(7);
        assert!(!labels.contains(&LABEL_READY.to_string()));
        assert!(!labels.contains(&LABEL_WORKING.to_string()));

        // Done row + ready label re-added = a fresh row on the next claim.
        forge.add_label(7, LABEL_READY).await.unwrap();
        assert!(src.claim(&key, LOCAL_HOST).await.unwrap().is_some());
        let row = store.task_by_origin("proj", "github:7").unwrap().unwrap();
        assert_eq!(row.status, "claimed");

        // A held row refuses the claim.
        src.release(&key).await.unwrap();
        let row = store.task_by_origin("proj", "github:7").unwrap().unwrap();
        store.hold_task(row.id).unwrap();
        assert!(src.claim(&key, LOCAL_HOST).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn github_source_respects_hold_and_missing_trigger_labels() {
        use crate::forge::fake::FakeForge;
        use crate::forge::{LABEL_HOLD, LABEL_READY};

        let forge = Arc::new(FakeForge::with_issue(
            7,
            "T",
            "B",
            &[LABEL_READY, LABEL_HOLD],
        ));
        let store = Store::open_in_memory().unwrap();
        let src = GithubTaskSource::new(forge.clone(), store.clone(), "proj".into());
        assert!(
            src.claim(&TaskKey::Issue(7), LOCAL_HOST)
                .await
                .unwrap()
                .is_none(),
            "hold refuses the claim"
        );
        forge.remove_label(7, LABEL_HOLD).await.unwrap();
        forge.remove_label(7, LABEL_READY).await.unwrap();
        assert!(
            src.claim(&TaskKey::Issue(7), LOCAL_HOST)
                .await
                .unwrap()
                .is_none(),
            "a de-labeled issue is a benign race"
        );
    }
}
