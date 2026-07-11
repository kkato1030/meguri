//! Worktree reaper: reclaim worktrees whose issue is closed on the forge
//! ("Authority": the issue's lifecycle, not local state, decides when a
//! worktree may go). Shared by `meguri prune` and the watch-loop sweep;
//! pane lifecycle stays with #13.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::Deps;
use crate::forge::IssueState;
use crate::gitops;
use crate::mux::PaneId;

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
    gitops::prune_worktrees(&deps.project.repo_path).await.ok();
    let root = project_worktree_root(deps);
    let mut candidates = Vec::new();
    for wt in gitops::list_worktrees(&deps.project.repo_path).await? {
        let path = std::fs::canonicalize(&wt.path).unwrap_or(wt.path);
        if !path.starts_with(&root) {
            continue; // not managed by meguri (e.g. the primary checkout)
        }
        candidates.push(classify(deps, path, wt.branch).await?);
    }
    Ok(candidates)
}

async fn classify(deps: &Deps, path: PathBuf, branch: Option<String>) -> Result<Candidate> {
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

    let Some(issue_number) = issue else {
        return Ok(candidate(Verdict::Orphan));
    };
    // An active run owns its worktree; don't even ask the forge.
    if runs.iter().any(|r| r.status.is_active()) {
        return Ok(candidate(Verdict::ActiveRun));
    }
    match deps.forge.issue_state(issue_number).await {
        Ok(IssueState::Open) => return Ok(candidate(Verdict::Open)),
        Ok(IssueState::Closed) => {}
        Err(e) => {
            tracing::warn!("cannot resolve state of issue #{issue_number}: {e:#}");
            return Ok(candidate(Verdict::StateUnknown));
        }
    }
    // Closed, but a live pane in the worktree means an agent (or a human
    // investigating) still stands there — pane reclamation is #13's job.
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
    let pane = PaneId(pane.clone());
    if kind == deps.mux.kind().as_str() {
        return deps.mux.pane_alive(&pane).await.unwrap_or(false);
    }
    match crate::mux::from_kind(kind, &deps.config.mux.session) {
        Ok(mux) => mux.pane_alive(&pane).await.unwrap_or(false),
        Err(_) => false,
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
            Some(branch) => {
                match gitops::delete_branch(
                    &deps.project.repo_path,
                    branch,
                    &deps.project.default_branch,
                    force,
                )
                .await
                {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("keeping branch {branch} (not merged?): {e:#}");
                        false
                    }
                }
            }
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

/// Watch-poll sweep: reclaim closed-issue worktrees, never forcing. Dirty
/// worktrees are left for `meguri prune --force`.
pub async fn sweep(deps: &Deps) -> Result<()> {
    let candidates = plan(deps).await?;
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
