//! The planner loop: `meguri:plan` issue → investigate the repository →
//! lightweight spec (`docs/specs/issue-<N>.md`) → spec PR labeled
//! `meguri:spec-reviewing`. Spec-first is opt-in; `meguri:ready` issues keep
//! going straight to the worker.
//!
//! The spec PR and the implementation PR are the same PR: after review (the
//! reviewer loop, or a human) flips the PR to `meguri:spec-ready`, the
//! spec-worker loop takes over the branch and stacks implementation commits
//! on it (issue #21). Branch naming, run
//! bookkeeping, and escalation therefore follow the worker conventions
//! exactly — only the trigger label, prompt, spec-file verification, and PR
//! shape differ.
//!
//! Second normal ending — decompose (issue #24): when the agent finds the
//! issue too big for one spec, it ends the turn with `status: decompose` and
//! a `children` list; meguri (not the agent) files the sub-issues, wires
//! GitHub-native `blocked_by` dependencies, labels each child by size
//! (`meguri:ready` / `meguri:plan`), leaves the rationale as a comment on
//! the parent, and un-labels the parent. Decomposition is one level only: a
//! child asking to decompose again escalates to `meguri:needs-human`.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, NeedsHuman};
use super::{Deps, Target};
use crate::forge;
use crate::store::RunRecord;
use crate::turn::{ChildIssue, TurnResultFile};

/// `runs.loop_kind` value for planner runs.
pub const KIND: &str = "planner";

/// Where an issue's spec lives, relative to the repository root.
pub fn spec_rel_path(issue: i64) -> String {
    format!("docs/specs/issue-{issue}.md")
}

/// Machine-readable marker embedded in every decomposition child's body; its
/// presence is the one-level guard (a child never decomposes again).
pub const DECOMPOSED_MARKER: &str = "<!-- meguri:decomposed-child -->";

/// Footer appended to every child issue's body: the human-visible parent
/// reference plus the machine marker.
pub fn decompose_child_footer(parent: i64) -> String {
    format!(
        "\n\n---\nParent issue: #{parent} (split out by meguri's planner)\n\n{DECOMPOSED_MARKER}"
    )
}

/// Was this issue created by a planner decomposition?
pub fn is_decomposed_child(issue_body: &str) -> bool {
    issue_body.contains(DECOMPOSED_MARKER)
}

/// The planner as a schedulable loop: `meguri:plan` issues in, spec PRs out.
pub struct PlannerLoop;

#[async_trait]
impl super::Loop for PlannerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        flow::discover_by_label(deps, KIND, forge::LABEL_PLAN).await
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_planner(deps, run_id).await
    }
}

pub async fn run_planner(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &PlannerFlavor).await
}

struct PlannerFlavor;

#[async_trait]
impl Flavor for PlannerFlavor {
    fn trigger_label(&self) -> &'static str {
        forge::LABEL_PLAN
    }

    fn execute_prompt(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> String {
        format!(
            "You are planning GitHub issue #{number} in this repository \
             (branch `{branch}`, a dedicated worktree). Write a spec for the \
             issue; do NOT implement it.\n\n\
             # Issue: {title}\n\n{body}\n\n\
             # Instructions\n\
             - Investigate the repository first: what the issue needs, which \
               files/modules it touches, and which decisions have to be made.\n\
             - Write the spec to `{spec}` (create parent directories as needed). \
               Keep it lightweight — it exists to converge review on the approach \
               before implementation: acceptance criteria, files to touch, and \
               key decisions are enough.\n\
             - A decision worth keeping after the PR merges belongs in an ADR \
               (`docs/adr/NNNN-<slug>.md`, next free number), not the spec.\n\
             - Do NOT implement the issue; the spec (plus any ADR) is the only \
               deliverable. The implementation continues later on this same branch.\n\
             - COMMIT your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push and do NOT create a pull request; meguri handles both.\n\
             - Do NOT switch branches or touch other worktrees.\n\n\
             {decompose_section}\
             {pr_section}{lang_section}",
            number = run.issue_number,
            branch = run.branch.as_deref().unwrap_or("?"),
            title = cp.issue_title,
            body = cp.issue_body,
            spec = spec_rel_path(run.issue_number),
            decompose_section = decompose_instruction(&cp.issue_body),
            pr_section = flow::pr_body_instruction(worktree),
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn.
    }

    /// The planner's deliverable is the spec file; committed-but-specless
    /// work gets a corrective turn.
    fn verify_work(&self, run: &RunRecord, worktree: &Path) -> std::result::Result<(), String> {
        let spec = spec_rel_path(run.issue_number);
        if worktree.join(&spec).is_file() {
            Ok(())
        } else {
            Err(format!(
                "- spec file `{spec}` does not exist (write it and commit it)"
            ))
        }
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        format!("Spec: {} (#{})", cp.issue_title, run.issue_number)
    }

    /// Label transition: the issue's `meguri:plan` becomes the PR's
    /// `meguri:spec-reviewing` — the PR is the reviewable artifact from here
    /// on. The PR label is load-bearing (review discovery keys off it), so
    /// failing to apply it fails the run instead of passing silently.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        if let Some(pr) = cp.pr_number {
            deps.forge
                .add_pr_label(pr, forge::LABEL_SPEC_REVIEWING)
                .await?;
        }
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_PLAN)
            .await
            .ok();
        Ok(())
    }

    /// Decompose handoff (issue #24): meguri files the sub-issues the agent
    /// described, wires `blocked_by`, labels each child by size, leaves the
    /// rationale on the parent, and un-labels the parent — it is not
    /// implemented by meguri itself. Runaway guard: decomposition is one
    /// level only, so a decomposition child asking to decompose again
    /// escalates to a human instead.
    async fn on_decompose(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        result: &TurnResultFile,
    ) -> Result<WorkerOutcome> {
        if is_decomposed_child(&cp.issue_body) {
            return Err(NeedsHuman(format!(
                "issue #{} is itself a decomposition child but the planner \
                 wants to decompose it again (only one level is allowed): {}",
                run.issue_number, result.summary
            ))
            .into());
        }
        if let Err(problem) = validate_children(&result.children) {
            return Err(NeedsHuman(format!(
                "agent returned an invalid decomposition for issue #{}: {problem}",
                run.issue_number
            ))
            .into());
        }

        // File the children in order so dependency indices resolve to
        // numbers; every child body carries the parent reference + marker.
        let mut numbers: Vec<i64> = Vec::with_capacity(result.children.len());
        for child in &result.children {
            let label = child_label(child);
            let body = format!(
                "{}{}",
                child.body.trim(),
                decompose_child_footer(run.issue_number)
            );
            let number = deps
                .forge
                .create_issue(&child.title, &body, &[label])
                .await?;
            // Sibling dependencies: the dependency gate (issue #23) keys off
            // these, so they decide the implementation order.
            for &dep in &child.blocked_by {
                deps.forge.add_blocked_by(number, numbers[dep]).await?;
            }
            // Parent-child dependency: the parent visibly waits for every
            // child on the forge's graph.
            deps.forge.add_blocked_by(run.issue_number, number).await?;
            numbers.push(number);
        }

        // Rationale on the parent ("Authority": the durable record of why —
        // and into what — the issue was split lives on the forge).
        let listing = result
            .children
            .iter()
            .zip(&numbers)
            .map(|(child, number)| {
                format!("- #{number} (`{}`) {}", child_label(child), child.title)
            })
            .collect::<Vec<_>>()
            .join("\n");
        deps.forge
            .comment(
                run.issue_number,
                &format!(
                    "🔁 **meguri**: this issue is too big for one spec, so the \
                     planner split it into sub-issues:\n\n{listing}\n\n> {}\n\n\
                     Ordering is enforced via `blocked_by`. This parent issue \
                     is not implemented by meguri itself — once all children \
                     are completed, a human decides whether it needs its own \
                     small spec or can simply be closed.",
                    result.summary
                ),
            )
            .await?;

        // The parent leaves the planner queue. Dropping the plan label is
        // load-bearing (if it lingered, the next poll would decompose
        // again); the claim release is best-effort as everywhere else.
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_PLAN)
            .await?;
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
        deps.store.emit(
            Some(&run.id),
            "issue.decomposed",
            json!({ "issue": run.issue_number, "children": numbers }),
        )?;
        Ok(WorkerOutcome::Decomposed(result.summary.clone()))
    }
}

/// The trigger label a child enters the loops with, by declared size.
/// [`validate_children`] has already rejected anything else.
fn child_label(child: &ChildIssue) -> &'static str {
    match child.kind.as_str() {
        "plan" => forge::LABEL_PLAN,
        _ => forge::LABEL_READY,
    }
}

/// Reject malformed decompositions before touching the forge; the Err text
/// reaches the human via the escalation comment.
fn validate_children(children: &[ChildIssue]) -> std::result::Result<(), String> {
    if children.is_empty() {
        return Err("`children` is empty".into());
    }
    for (i, child) in children.iter().enumerate() {
        if child.title.trim().is_empty() {
            return Err(format!("child {i} has an empty title"));
        }
        if !matches!(child.kind.as_str(), "ready" | "plan") {
            return Err(format!(
                "child {i} has unknown kind `{}` (must be \"ready\" or \"plan\")",
                child.kind
            ));
        }
        for &dep in &child.blocked_by {
            if dep >= i {
                return Err(format!(
                    "child {i} is blocked_by {dep}, but dependencies may only \
                     reference earlier entries"
                ));
            }
        }
    }
    Ok(())
}

/// Prompt section inviting the decompose ending — except on issues that are
/// themselves decomposition children, where only one level is allowed and
/// the agent is told to hand a still-too-big issue to a human instead.
fn decompose_instruction(issue_body: &str) -> String {
    if is_decomposed_child(issue_body) {
        return "# Too big for one spec?\n\
                This issue was itself split out of a bigger issue by a previous \
                decomposition, and decomposition is one level only — do NOT \
                propose another split. If the issue is still too big for one \
                spec, end the turn with `\"status\": \"needs_human\"` and explain \
                why in `summary`.\n\n"
            .to_string();
    }
    "# Too big for one spec?\n\
     If your investigation shows the issue cannot converge on one spec and \
     must be split into sub-issues, do NOT write a spec and do NOT create \
     any issues yourself. Instead end the turn with `\"status\": \"decompose\"` \
     in the result file (accepted here in addition to the completion \
     contract's statuses), with:\n\
     - `summary`: one paragraph on why you split it this way (it becomes a \
       comment on the issue).\n\
     - `children`: the sub-issues to file, in dependency order. Each entry is \
       {\"title\": \"...\", \"body\": \"<minimal body>\", \"kind\": \"ready\"|\"plan\", \
       \"blocked_by\": [<zero-based indices of earlier entries it depends on>]}. \
       Use kind \"ready\" for children small enough to implement directly and \
       \"plan\" for children that still need their own design pass.\n\
     meguri files the sub-issues, wires the `blocked_by` dependencies, and \
     labels them. Decomposition is one level only: sub-issues cannot be \
     decomposed again.\n\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_path_is_per_issue() {
        assert_eq!(spec_rel_path(42), "docs/specs/issue-42.md");
    }

    #[test]
    fn verify_work_requires_the_spec_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = fake_run(7);
        run.worktree_path = Some(dir.path().to_string_lossy().into_owned());

        let err = PlannerFlavor.verify_work(&run, dir.path()).unwrap_err();
        assert!(err.contains("docs/specs/issue-7.md"), "{err}");

        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(dir.path().join("docs/specs/issue-7.md"), "# Spec\n").unwrap();
        assert!(PlannerFlavor.verify_work(&run, dir.path()).is_ok());
    }

    #[test]
    fn prompt_demands_spec_not_implementation() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            issue_body: "Cache the thing.".into(),
            ..Default::default()
        };
        let prompt = PlannerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("docs/specs/issue-7.md"));
        assert!(prompt.contains("do NOT implement"));
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains("# Pull request description"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_pins_output_language_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint::default();
        let mut deps = fake_deps();
        deps.config.language = Some("日本語".into());
        let prompt = PlannerFlavor.execute_prompt(&deps, &run, &cp, dir.path());
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    #[test]
    fn prompt_invites_decompose() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Huge feature".into(),
            issue_body: "Everything at once.".into(),
            ..Default::default()
        };
        let prompt = PlannerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("# Too big for one spec?"));
        assert!(prompt.contains(r#""status": "decompose""#));
        assert!(prompt.contains(r#""blocked_by""#));
    }

    #[test]
    fn prompt_forbids_decompose_on_decomposition_children() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Part 2".into(),
            issue_body: format!("Do the part.{}", decompose_child_footer(3)),
            ..Default::default()
        };
        let prompt = PlannerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("# Too big for one spec?"));
        assert!(prompt.contains("one level only"));
        assert!(!prompt.contains(r#""status": "decompose""#));
    }

    #[test]
    fn child_footer_references_parent_and_carries_marker() {
        let footer = decompose_child_footer(42);
        assert!(footer.contains("#42"));
        assert!(is_decomposed_child(&footer));
        assert!(!is_decomposed_child("Parent issue: #42 without the marker"));
    }

    #[test]
    fn validate_children_rejects_malformed_decompositions() {
        let child = |title: &str, kind: &str, blocked_by: Vec<usize>| ChildIssue {
            title: title.into(),
            body: String::new(),
            kind: kind.into(),
            blocked_by,
        };
        assert!(validate_children(&[]).unwrap_err().contains("empty"));
        assert!(
            validate_children(&[child("  ", "ready", vec![])])
                .unwrap_err()
                .contains("empty title")
        );
        assert!(
            validate_children(&[child("a", "huge", vec![])])
                .unwrap_err()
                .contains("unknown kind")
        );
        // Dependencies may only point backwards (no self/forward references).
        assert!(
            validate_children(&[child("a", "ready", vec![0])])
                .unwrap_err()
                .contains("earlier")
        );
        assert!(
            validate_children(&[child("a", "ready", vec![]), child("b", "plan", vec![0]),]).is_ok()
        );
    }

    #[test]
    fn pr_title_carries_spec_prefix() {
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        assert_eq!(PlannerFlavor.pr_title(&run, &cp), "Spec: Add caching (#7)");
    }

    fn fake_run(issue: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, issue, "t").unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.branch = Some("meguri/test".into());
        run
    }

    fn fake_deps() -> Deps {
        use std::sync::Arc;
        Deps {
            store: crate::store::Store::open_in_memory().unwrap(),
            mux: Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge: Arc::new(crate::forge::fake::FakeForge::default()),
            config: crate::config::Config::default(),
            project: crate::config::ProjectConfig {
                id: "proj".into(),
                repo_path: "/tmp/unused".into(),
                repo_slug: "me/proj".into(),
                default_branch: "main".into(),
                language: None,
                check_command: None,
                worktree_root: None,
                pr: None,
            },
        }
    }
}
