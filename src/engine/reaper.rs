//! The reaper: reclaim panes and worktrees whose issue is closed on the
//! forge ("Authority": the issue's lifecycle, not local state, decides when
//! a pane/worktree may go). Shared by `meguri prune` and the watch-loop
//! sweep. Reclamation is reversible: the agent's native session id is saved
//! before a pane is killed, so `claude --resume <id>` restores the context.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;

use super::Deps;
use crate::agent_session;
use crate::forge::IssueState;
use crate::gitops;
use crate::mux::{Multiplexer, PaneId};
use crate::store::{PaneRecord, ROLE_AUTHOR, ROLE_IMPL_REVIEW, ROLE_REVIEW};

/// Reclamation reason for a pane whose mapping outlived the pane itself.
pub const REASON_PANE_DEAD: &str = "pane-dead";
/// Reclamation reason for a pane whose issue closed on the forge.
pub const REASON_ISSUE_CLOSED: &str = "issue-closed";

/// Why a meguri worktree is (not) reclaimable right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Issue closed, no live pane, tree clean: safe to remove.
    Reclaim,
    /// Issue is still open — never touched.
    Open,
    /// An active run owns this worktree (skipped without a forge call).
    ActiveRun,
    /// Issue closed but an agent pane still sits in this worktree.
    PaneAlive,
    /// Issue closed but the tree has uncommitted changes (`--force` overrides).
    Dirty,
    /// No issue can be resolved from the branch name or the runs table.
    Orphan,
    /// The forge could not tell us the issue state — assume nothing.
    StateUnknown,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reclaim => "reclaim",
            Self::Open => "open issue",
            Self::ActiveRun => "active run",
            Self::PaneAlive => "pane alive",
            Self::Dirty => "dirty",
            Self::Orphan => "orphan",
            Self::StateUnknown => "state unknown",
        }
    }

    fn reclaimable(&self, force: bool) -> bool {
        matches!(self, Self::Reclaim) || (force && matches!(self, Self::Dirty))
    }
}

/// One meguri-managed worktree and what the reaper decided about it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub issue: Option<i64>,
    pub run_id: Option<String>,
    pub verdict: Verdict,
}

/// A worktree the reaper actually removed.
#[derive(Debug, Clone)]
pub struct Reclaimed {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub issue: Option<i64>,
    /// False when the branch was kept (unmerged and not forced).
    pub branch_deleted: bool,
}

/// Per-sweep cache of issue states so panes and worktrees of the same issue
/// cost one forge call, not two (discovery already polls; keep extra `gh`
/// calls minimal). `None` = the forge could not tell us.
#[derive(Default)]
pub struct IssueStates(HashMap<i64, Option<IssueState>>);

impl IssueStates {
    async fn get(&mut self, deps: &Deps, issue: i64) -> Option<IssueState> {
        if let Some(state) = self.0.get(&issue) {
            return *state;
        }
        let state = match deps.forge().issue_state(issue).await {
            Ok(state) => Some(state),
            Err(e) => {
                tracing::warn!("cannot resolve state of issue #{issue}: {e:#}");
                None
            }
        };
        self.0.insert(issue, state);
        state
    }
}

/// Resolve the mux a persisted `mux_kind` refers to, reusing the live handle
/// when it matches (also keeps tests on their FakeMux).
fn mux_for(deps: &Deps, kind: &str) -> Option<Arc<dyn Multiplexer>> {
    if kind == deps.mux.kind().as_str() {
        return Some(deps.mux.clone());
    }
    // Cross-kind fallback (a persisted pane on a different mux than we run):
    // only ever kills/reads by pane id, so the base label suffices.
    crate::mux::from_kind(kind, &deps.config.mux.session, None).ok()
}

/// The directory holding this project's worktrees, canonicalized so paths
/// from `git worktree list` (which resolves symlinks) compare correctly.
fn project_worktree_root(deps: &Deps) -> PathBuf {
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root)
        .join(&deps.project.id);
    std::fs::canonicalize(&root).unwrap_or(root)
}

/// Enumerate this project's meguri worktrees and classify each one.
/// Prunes stale worktree registrations first so the listing is honest.
pub async fn plan(deps: &Deps) -> Result<Vec<Candidate>> {
    plan_with(deps, &mut IssueStates::default()).await
}

/// [`plan`] sharing an issue-state cache with a pane sweep of the same tick.
pub async fn plan_with(deps: &Deps, states: &mut IssueStates) -> Result<Vec<Candidate>> {
    gitops::prune_worktrees(&deps.project.repo_path).await.ok();
    let root = project_worktree_root(deps);
    let mut candidates = Vec::new();
    for wt in gitops::list_worktrees(&deps.project.repo_path).await? {
        let path = std::fs::canonicalize(&wt.path).unwrap_or(wt.path);
        if !path.starts_with(&root) {
            continue; // not managed by meguri (e.g. the primary checkout)
        }
        candidates.push(classify(deps, states, path, wt.branch).await?);
    }
    Ok(candidates)
}

async fn classify(
    deps: &Deps,
    states: &mut IssueStates,
    path: PathBuf,
    branch: Option<String>,
) -> Result<Candidate> {
    let runs = deps.store.runs_for_worktree(
        &deps.project.id,
        branch.as_deref(),
        &path.to_string_lossy(),
    )?;
    let issue = branch
        .as_deref()
        .and_then(gitops::issue_from_branch)
        .or_else(|| runs.first().map(|r| r.issue_number));
    let run_id = runs.first().map(|r| r.id.clone());

    let candidate = |verdict| Candidate {
        path: path.clone(),
        branch: branch.clone(),
        issue,
        run_id: run_id.clone(),
        verdict,
    };

    // An active run owns its worktree; don't even ask the forge.
    if runs.iter().any(|r| r.status.is_active()) {
        return Ok(candidate(Verdict::ActiveRun));
    }
    // Local mode (no forge): there is no issue lifecycle to consult, and a
    // `deliver = "branch"` deliverable *is* the branch + worktree — so the
    // Phase 1 reaper never auto-reclaims it. It parks as StateUnknown until
    // `meguri accept` (issue #54 Phase 2) designs the reclaim conditions.
    if deps.forge.is_none() {
        return Ok(candidate(Verdict::StateUnknown));
    }

    let Some(issue_number) = issue else {
        return Ok(candidate(Verdict::Orphan));
    };
    match states.get(deps, issue_number).await {
        Some(IssueState::Open) => return Ok(candidate(Verdict::Open)),
        Some(IssueState::Closed) => {}
        None => return Ok(candidate(Verdict::StateUnknown)),
    }
    // Closed, but a live pane in the worktree means an agent (or a human
    // investigating) still stands there. The pane sweep of the same tick
    // runs first, so this only trips when the kill failed (or was skipped);
    // the worktree then waits for the next sweep. Both lanes of the issue
    // are checked — either one alive keeps the worktree.
    for role in [ROLE_AUTHOR, ROLE_REVIEW, ROLE_IMPL_REVIEW] {
        if let Some(pane) = deps.store.get_pane(&deps.project.id, issue_number, role)?
            && record_pane_alive(deps, &pane).await
        {
            return Ok(candidate(Verdict::PaneAlive));
        }
    }
    for run in &runs {
        if pane_alive(deps, run).await {
            return Ok(candidate(Verdict::PaneAlive));
        }
    }
    match gitops::status_clean(&path).await {
        Ok(true) => Ok(candidate(Verdict::Reclaim)),
        Ok(false) => Ok(candidate(Verdict::Dirty)),
        Err(e) => {
            tracing::warn!("cannot read status of {}: {e:#}", path.display());
            Ok(candidate(Verdict::StateUnknown))
        }
    }
}

async fn pane_alive(deps: &Deps, run: &crate::store::RunRecord) -> bool {
    let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id) else {
        return false;
    };
    match mux_for(deps, kind) {
        Some(mux) => mux.pane_alive(&PaneId(pane.clone())).await.unwrap_or(false),
        None => false,
    }
}

async fn record_pane_alive(deps: &Deps, pane: &PaneRecord) -> bool {
    let (Some(kind), Some(id)) = (&pane.mux_kind, &pane.mux_pane_id) else {
        return false;
    };
    match mux_for(deps, kind) {
        Some(mux) => mux.pane_alive(&PaneId(id.clone())).await.unwrap_or(false),
        None => false,
    }
}

/// Remove every reclaimable candidate: `git worktree remove` + branch
/// deletion (merged-only unless `force`) + a final `git worktree prune`.
pub async fn reclaim(deps: &Deps, candidates: &[Candidate], force: bool) -> Result<Vec<Reclaimed>> {
    let mut reclaimed = Vec::new();
    let mut fetched = false;
    for c in candidates.iter().filter(|c| c.verdict.reclaimable(force)) {
        // The merged-branch check compares against origin/<default>; make it
        // current once per reclaim pass (best-effort, offline still works).
        if !fetched {
            fetched = true;
            gitops::run_git(
                &deps.project.repo_path,
                &["fetch", "origin", &deps.project.default_branch],
            )
            .await
            .ok();
        }
        if let Err(e) = gitops::remove_worktree(&deps.project.repo_path, &c.path).await {
            tracing::warn!("cannot remove worktree {}: {e:#}", c.path.display());
            continue;
        }
        let branch_deleted = match &c.branch {
            Some(branch) => delete_branch_if_merged(deps, branch, force).await,
            None => false,
        };
        deps.store.emit(
            c.run_id.as_deref(),
            "worktree.reclaimed",
            json!({
                "path": c.path.to_string_lossy(),
                "branch": c.branch,
                "issue": c.issue,
                "branch_deleted": branch_deleted,
                "forced": force && c.verdict == Verdict::Dirty,
            }),
        )?;
        reclaimed.push(Reclaimed {
            path: c.path.clone(),
            branch: c.branch.clone(),
            issue: c.issue,
            branch_deleted,
        });
    }
    if !reclaimed.is_empty() {
        gitops::prune_worktrees(&deps.project.repo_path).await.ok();
    }
    Ok(reclaimed)
}

/// Delete a reclaimed candidate's branch; true when it was deleted. Two
/// merged-ness checks complement each other: the offline `--is-ancestor`
/// check inside [`gitops::delete_branch`] first, and when that says
/// unmerged, the forge's PR state — a squash or rebase merge rewrites the
/// commits, so the branch tip never becomes an ancestor of the base, but
/// the PR still reads `merged`. Either verdict deletes; no PR, an open PR,
/// or a failed forge lookup keeps the branch, as before.
async fn delete_branch_if_merged(deps: &Deps, branch: &str, force: bool) -> bool {
    let ancestor_err = match gitops::delete_branch(
        &deps.project.repo_path,
        branch,
        &deps.project.default_branch,
        force,
    )
    .await
    {
        Ok(()) => return true,
        Err(e) if force => {
            // force already skips the merged check; nothing left to try.
            tracing::warn!("keeping branch {branch}: {e:#}");
            return false;
        }
        Err(e) => e,
    };
    // No forge (local mode): the offline ancestor check is all we have, and it
    // said unmerged — keep the branch (its verified commits are the deliverable).
    let Some(forge) = deps.forge.as_ref() else {
        tracing::warn!("keeping branch {branch}: no forge to check PR merge state (local mode)");
        return false;
    };
    match forge.pr_for_branch(branch).await {
        Ok(Some(pr)) if pr.state == "merged" => {
            match gitops::delete_branch(
                &deps.project.repo_path,
                branch,
                &deps.project.default_branch,
                true,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(
                        "deleted branch {branch} (PR #{} merged on the forge)",
                        pr.number
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!("keeping branch {branch}: {e:#}");
                    false
                }
            }
        }
        Ok(_) => {
            tracing::warn!("keeping branch {branch} (not merged?): {ancestor_err:#}");
            false
        }
        Err(e) => {
            tracing::warn!(
                "keeping branch {branch} (not merged locally, forge lookup failed: {e:#})"
            );
            false
        }
    }
}

/// One lane↔pane mapping and what the reaper decided about it.
#[derive(Debug, Clone)]
pub struct PaneCandidate {
    pub issue: i64,
    pub role: String,
    pub pane: PaneId,
    pub worktree_path: Option<String>,
    pub verdict: Verdict,
    /// Why a `Reclaim` verdict was reached ([`REASON_PANE_DEAD`] or
    /// [`REASON_ISSUE_CLOSED`]); carried into the `pane.reclaimed` event.
    pub reason: &'static str,
}

/// A pane the reaper released (killed and detached from its lane).
#[derive(Debug, Clone)]
pub struct ReclaimedPane {
    pub issue: i64,
    pub role: String,
    pub pane: PaneId,
    /// Agent session saved before the kill; `claude --resume <id>` restores
    /// the context.
    pub agent_session_id: Option<String>,
}

/// Classify every lane↔pane mapping of the project: reclaim when the issue
/// closed (and no run is active), or when the pane already died and only the
/// stale mapping is left.
pub async fn plan_panes(deps: &Deps, states: &mut IssueStates) -> Result<Vec<PaneCandidate>> {
    let mut candidates = Vec::new();
    for record in deps.store.list_panes(&deps.project.id)? {
        let Some(id) = record.mux_pane_id.clone() else {
            continue;
        };
        let candidate = |verdict, reason| PaneCandidate {
            issue: record.issue_number,
            role: record.role.clone(),
            pane: PaneId(id.clone()),
            worktree_path: record.worktree_path.clone(),
            verdict,
            reason,
        };
        // A dead pane is a stale mapping whatever the issue state; clear it
        // (saving what context we can) without a forge call.
        if !record_pane_alive(deps, &record).await {
            candidates.push(candidate(Verdict::Reclaim, REASON_PANE_DEAD));
            continue;
        }
        if deps
            .store
            .issue_has_active_run(&deps.project.id, record.issue_number)?
        {
            candidates.push(candidate(Verdict::ActiveRun, ""));
            continue;
        }
        candidates.push(match states.get(deps, record.issue_number).await {
            Some(IssueState::Open) => candidate(Verdict::Open, ""),
            Some(IssueState::Closed) => candidate(Verdict::Reclaim, REASON_ISSUE_CLOSED),
            None => candidate(Verdict::StateUnknown, ""),
        });
    }
    Ok(candidates)
}

/// Release every reclaimable pane: save the agent's native session id, kill
/// the pane, detach the mapping (the saved session id survives for resume).
pub async fn reclaim_panes(
    deps: &Deps,
    candidates: &[PaneCandidate],
) -> Result<Vec<ReclaimedPane>> {
    let mut reclaimed = Vec::new();
    for c in candidates.iter().filter(|c| c.verdict == Verdict::Reclaim) {
        reclaimed.push(release_pane_record(deps, c.issue, &c.role, c.reason).await?);
    }
    Ok(reclaimed)
}

/// Release one lane's pane outside a sweep (`meguri stop`, `keep_pane =
/// "never"`, worktree moved). No-op returning None when the lane has no
/// pane mapping.
pub async fn release_pane(
    deps: &Deps,
    issue: i64,
    role: &str,
    reason: &str,
) -> Option<ReclaimedPane> {
    match deps.store.get_pane(&deps.project.id, issue, role) {
        Ok(Some(record)) if record.mux_pane_id.is_some() => {
            match release_pane_record(deps, issue, role, reason).await {
                Ok(reclaimed) => Some(reclaimed),
                Err(e) => {
                    tracing::warn!("cannot release {role} pane of issue #{issue}: {e:#}");
                    None
                }
            }
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("cannot look up {role} pane of issue #{issue}: {e:#}");
            None
        }
    }
}

async fn release_pane_record(
    deps: &Deps,
    issue: i64,
    role: &str,
    reason: &str,
) -> Result<ReclaimedPane> {
    let record = deps
        .store
        .get_pane(&deps.project.id, issue, role)?
        .with_context(|| format!("issue #{issue} has no {role} pane record"))?;
    let id = record
        .mux_pane_id
        .clone()
        .with_context(|| format!("issue #{issue} has no live {role} pane"))?;
    let pane = PaneId(id);

    // Reversibility first: persist the agent's native session id before the
    // pane goes, so an early reclaim or a reopened issue can resume. The
    // turn path already saves it after every completed turn; this is the
    // last-resort net for panes that die mid-turn.
    let session_root = agent_session::session_root(&deps.config.agent);
    let agent_session_id = record
        .worktree_path
        .as_deref()
        .and_then(|wt| agent_session::latest_session_id(&session_root, Path::new(wt)));
    if let Some(session) = &agent_session_id {
        deps.store
            .save_pane_session(&deps.project.id, issue, role, Some(session))?;
    }

    if let Some(kind) = &record.mux_kind
        && let Some(mux) = mux_for(deps, kind)
        && mux.pane_alive(&pane).await.unwrap_or(false)
        && let Err(e) = mux.kill_pane(&pane).await
    {
        tracing::warn!("cannot kill pane {pane} of issue #{issue}: {e:#}");
        anyhow::bail!("kill_pane failed for issue #{issue}: {e}");
    }
    deps.store
        .mark_pane_reclaimed(&deps.project.id, issue, role)?;
    deps.store.emit(
        None,
        "pane.reclaimed",
        json!({
            "issue": issue,
            "role": role,
            "pane": pane.0,
            "reason": reason,
            "agent_session_id": agent_session_id,
        }),
    )?;
    Ok(ReclaimedPane {
        issue,
        role: role.to_string(),
        pane,
        agent_session_id,
    })
}

/// Watch-poll sweep: reclaim panes and worktrees of closed issues, never
/// forcing. Panes go first so their worktrees become reclaimable in the same
/// tick; dirty worktrees are left for `meguri prune --force`.
pub async fn sweep(deps: &Deps) -> Result<()> {
    let mut states = IssueStates::default();
    for p in reclaim_panes(deps, &plan_panes(deps, &mut states).await?).await? {
        tracing::info!(
            "reclaimed {} pane {} (issue #{}{})",
            p.role,
            p.pane,
            p.issue,
            match &p.agent_session_id {
                Some(id) => format!(", session {id} saved"),
                None => ", no session found".to_string(),
            },
        );
    }
    let candidates = plan_with(deps, &mut states).await?;
    for c in candidates.iter().filter(|c| c.verdict == Verdict::Dirty) {
        tracing::warn!(
            "worktree {} has uncommitted changes for closed issue #{} — \
             skipped (reclaim with `meguri prune --force`)",
            c.path.display(),
            c.issue.unwrap_or(0),
        );
    }
    for r in reclaim(deps, &candidates, false).await? {
        tracing::info!(
            "reclaimed worktree {} (issue #{}, branch {}{})",
            r.path.display(),
            r.issue.unwrap_or(0),
            r.branch.as_deref().unwrap_or("-"),
            if r.branch_deleted {
                ", deleted"
            } else {
                ", kept"
            },
        );
    }
    Ok(())
}

/// Recursive on-disk size of a directory (symlinks not followed).
pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.metadata() {
            Ok(md) if md.is_dir() => dir_size(&e.path()),
            Ok(md) if md.is_file() => md.len(),
            _ => 0,
        })
        .sum()
}
