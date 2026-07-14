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
//! The spec itself is transient scaffolding: the spec-worker prunes it when
//! the implementation lands, so it never reaches the default branch (issue
//! #48). Anything with durable value is routed out of the spec by the prompt
//! — design decisions to ADRs, domain rules to permanent domain documents.
//!
//! Lifetime (issue #92): keyed by the issue, new branch and worktree, pane
//! in the issue's author lane — kept after the spec PR opens, so the spec
//! worker (and later fixer rounds) continue in the same live session; the
//! reaper reclaims it when the issue closes.
//!
//! Second normal ending — decompose (issue #24): when the agent finds the
//! issue too big for one spec, it ends the turn with `status: decompose` and
//! a `children` list; meguri (not the agent) files the sub-issues, wires
//! GitHub-native `blocked_by` dependencies, labels each child by size
//! (`meguri:ready` / `meguri:plan`), leaves the rationale as a comment on
//! the parent, and un-labels the parent. Decomposition is one level only: a
//! child asking to decompose again escalates to `meguri:needs-human`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, Kind, NeedsHuman};
use super::{Deps, Target};
use crate::forge::{self, Forge};
use crate::store::RunRecord;
use crate::tasks::TaskKind;
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
/// reference plus the machine marker. Same-repo convenience over
/// [`decompose_child_footer_ref`].
pub fn decompose_child_footer(parent: i64) -> String {
    decompose_child_footer_ref(&format!("#{parent}"))
}

/// Footer with an already-formatted parent reference — `#N` within the parent's
/// repo, or `owner/repo#N` when the child lives in a workspace sibling so the
/// link resolves across repos (issue #154).
pub fn decompose_child_footer_ref(parent_ref: &str) -> String {
    format!(
        "\n\n---\nParent issue: {parent_ref} (split out by meguri's planner)\n\n{DECOMPOSED_MARKER}"
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
        // The planner ships a spec *PR*, so it needs a forge. Local mode has
        // no planner yet (issue #54 Phase 3): a local `plan` task stays
        // queued and dormant rather than being driven into a forge call.
        if deps.forge.is_none() {
            return Ok(Vec::new());
        }
        Ok(deps
            .task_source
            .discover(TaskKind::Plan)
            .await?
            .into_iter()
            .map(|t| Target {
                key: t.key,
                title: t.title,
            })
            .collect())
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

    /// The plan side of the symmetric loop (ADR 0008).
    fn kind(&self) -> Kind {
        Kind::Plan
    }

    /// The planner self-reviews its own spec/ADR before opening the spec PR
    /// (ADR 0008): the internal multi-lens review→fix loop runs in the run's
    /// worktree with no forge calls, symmetric with the worker.
    fn self_reviews(&self) -> bool {
        true
    }

    /// The spec PR closes its issue only under combined delivery (where it
    /// morphs into the implementation PR). Under separate delivery it is a
    /// standalone PR that merges on its own, so it uses a non-closing `Refs #N`
    /// and the handoff sweep advances the issue instead (ADR 0008 §6).
    fn pr_closes_issue(&self, deps: &Deps) -> bool {
        deps.project.plan_delivery == crate::config::PlanDelivery::Combined
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
             - The spec is disposable scaffolding: it is deleted when the \
               implementation lands and never survives the merge. Anything \
               with durable value therefore belongs in a structured permanent \
               document, not the spec:\n\
               - a design decision (why this approach) goes in an ADR \
                 (`docs/adr/NNNN-<slug>.md`, next free number);\n\
               - a business rule / domain invariant the system must keep \
                 satisfying goes in the permanent domain document the \
                 repository already uses (create one only if this issue \
                 introduces such rules and no such document exists yet).\n\
             - Do NOT implement the issue; the spec (plus any ADR) is the only \
               deliverable. The implementation continues later on this same branch.\n\
             - COMMIT your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push and do NOT create a pull request; meguri handles both.\n\
             - Do NOT switch branches or touch other worktrees.\n\n\
             {depth_section}\
             {decompose_section}\
             {pr_section}{lang_section}",
            number = run.issue_number,
            branch = run.branch.as_deref().unwrap_or("?"),
            title = cp.issue_title,
            body = cp.issue_body,
            spec = spec_rel_path(run.issue_number),
            depth_section = adaptive_depth_instruction(&cp.issue_body),
            decompose_section = decompose_instruction(deps, &cp.issue_body),
            pr_section = flow::pr_body_instruction(worktree),
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn.
    }

    /// The planner's deliverable is the spec file; committed-but-specless
    /// work gets a corrective turn.
    fn verify_work(
        &self,
        run: &RunRecord,
        _cp: &Checkpoint,
        worktree: &Path,
    ) -> std::result::Result<(), String> {
        let spec = spec_rel_path(run.issue_number);
        if worktree.join(&spec).is_file() {
            Ok(())
        } else {
            Err(format!(
                "- spec file `{spec}` does not exist (write it and commit it)"
            ))
        }
    }

    /// The `Spec:` prefix hack is retired (issue #136): the planner's own
    /// execute turn sets `cp.subject` to what it actually did (e.g. "Write a
    /// spec for ..."), so the PR title reads honestly without a mechanical
    /// prefix. Falls back to the issue title when the agent omitted
    /// `subject` (backward compatibility).
    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        flow::default_pr_title(run, cp)
    }

    /// Label transition (ADR 0005): the PR gets `meguri:spec-reviewing` (it is
    /// the reviewable artifact from here on) and the issue's phase moves from
    /// `meguri:plan` to `meguri:speccing` (a spec PR is now open). Both adds
    /// are load-bearing — the PR label backs review discovery, the issue's
    /// phase label backs the "unlabeled = untriaged" invariant — so failing
    /// either fails the run; the `plan` / `working` removals stay best-effort.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        if let Some(pr) = cp.pr_number {
            // The plan guard is the reviewer gate: with it on, the spec PR
            // enters `spec-reviewing` (the guard flips it to `spec-ready` when
            // clean); with it off, no one would flip it, so the PR opens
            // straight at `spec-ready` — the internal self-review is the only
            // gate — and the state machine never deadlocks (ADR 0008 §3).
            let guard_on = deps.config.review_for(&deps.project).guard.plan;
            let pr_label = if guard_on {
                forge::LABEL_SPEC_REVIEWING
            } else {
                forge::LABEL_SPEC_READY
            };
            deps.forge().add_pr_label(pr, pr_label).await?;
        }
        deps.forge()
            .add_label(run.issue_number, forge::LABEL_SPECCING)
            .await?;
        deps.forge()
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
        deps.forge()
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

        // Issue-filing scope (issue #154 / ADR 0009): a child may target the
        // parent's own project or any of its workspace siblings — nothing
        // else. Keeping the scope in config (not the issue body) preserves the
        // "who decides scope = host operator" boundary.
        let parent_id = deps.project.id.clone();
        let siblings = deps.config.workspace_siblings(&parent_id);
        let mut allowed: Vec<&str> = vec![parent_id.as_str()];
        allowed.extend(siblings.iter().map(|p| p.id.as_str()));

        if let Err(problem) = validate_children(&result.children, &allowed) {
            return Err(NeedsHuman(format!(
                "agent returned an invalid decomposition for issue #{}: {problem}",
                run.issue_number
            ))
            .into());
        }

        let parent_slug = deps.project.repo_slug.clone().ok_or_else(|| {
            anyhow::Error::from(NeedsHuman(format!(
                "project {parent_id:?} has no repo_slug, so issue #{} cannot be decomposed",
                run.issue_number
            )))
        })?;

        // File the children in order so dependency indices resolve to
        // numbers; every child body carries the parent reference + marker.
        // `numbers`/`slugs` are parallel to `result.children` so a cross-repo
        // `blocked_by` can name the exact repo each blocker lives in.
        let mut numbers: Vec<i64> = Vec::with_capacity(result.children.len());
        let mut slugs: Vec<String> = Vec::with_capacity(result.children.len());
        for child in &result.children {
            let (forge, slug) = resolve_child_target(deps, &parent_slug, child)?;
            let labels: Vec<&str> = child_label(child).into_iter().collect();
            // Qualify the parent reference for a cross-repo child so its link
            // resolves back to the parent's repo, not a same-numbered issue in
            // the child's repo.
            let parent_ref = if slug == parent_slug {
                format!("#{}", run.issue_number)
            } else {
                format!("{parent_slug}#{}", run.issue_number)
            };
            let body = format!(
                "{}{}",
                child.body.trim(),
                decompose_child_footer_ref(&parent_ref)
            );
            let number = forge.create_issue(&child.title, &body, &labels).await?;
            // Sibling dependencies: the dependency gate (issue #23) keys off
            // these, so they decide the implementation order. The blocker may
            // live in another repo, so name it by its slug.
            for &dep in &child.blocked_by {
                forge
                    .add_blocked_by_in(number, &slugs[dep], numbers[dep])
                    .await?;
            }
            // Parent-child dependency: the parent (always in its own repo)
            // visibly waits for every child on the forge's graph.
            deps.forge()
                .add_blocked_by_in(run.issue_number, &slug, number)
                .await?;
            numbers.push(number);
            slugs.push(slug);
        }

        // Rationale on the parent ("Authority": the durable record of why —
        // and into what — the issue was split lives on the forge). Cross-repo
        // children are rendered `owner/repo#N` so the reference resolves.
        let listing = result
            .children
            .iter()
            .zip(&numbers)
            .zip(&slugs)
            .map(|((child, number), slug)| {
                let reference = if *slug == parent_slug {
                    format!("#{number}")
                } else {
                    format!("{slug}#{number}")
                };
                format!("- {reference} (`{}`) {}", child.kind, child.title)
            })
            .collect::<Vec<_>>()
            .join("\n");
        deps.forge()
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
        deps.forge()
            .remove_label(run.issue_number, forge::LABEL_PLAN)
            .await?;
        deps.forge()
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

/// The trigger label a child enters the loops with, by declared kind.
/// `None` for a `human` node: it is filed with no trigger label, so discovery
/// never drives it and a human closes it (issue #154).
/// [`validate_children`] has already rejected any other kind.
fn child_label(child: &ChildIssue) -> Option<&'static str> {
    match child.kind.as_str() {
        "human" => None,
        "plan" => Some(forge::LABEL_PLAN),
        _ => Some(forge::LABEL_READY),
    }
}

/// The forge and repo slug a child issue is filed into: the parent's own repo
/// when `project` is omitted (or names the parent), else a workspace sibling
/// resolved through [`Deps::forge_factory`] (issue #154). `validate_children`
/// has already confirmed the project is in scope; this additionally rejects a
/// sibling with no GitHub repo to file into (local mode).
fn resolve_child_target(
    deps: &Deps,
    parent_slug: &str,
    child: &ChildIssue,
) -> Result<(Arc<dyn Forge>, String)> {
    match child.project.as_deref() {
        None => Ok((deps.forge().clone(), parent_slug.to_string())),
        Some(pid) if pid == deps.project.id => Ok((deps.forge().clone(), parent_slug.to_string())),
        Some(pid) => {
            let sibling = deps.config.project(pid).ok_or_else(|| {
                anyhow::Error::from(NeedsHuman(format!(
                    "decomposition targets project {pid:?}, which is not defined"
                )))
            })?;
            let slug = sibling.repo_slug.clone().ok_or_else(|| {
                anyhow::Error::from(NeedsHuman(format!(
                    "decomposition targets project {pid:?}, but it is local-mode \
                     (no repository to file issues in)"
                )))
            })?;
            Ok((deps.forge_factory.for_slug(&slug), slug))
        }
    }
}

/// Reject malformed decompositions before touching the forge; the Err text
/// reaches the human via the escalation comment. `allowed_projects` is the
/// parent's own project id plus its workspace sibling ids — the only repos a
/// child may target (issue #154 / ADR 0009).
fn validate_children(
    children: &[ChildIssue],
    allowed_projects: &[&str],
) -> std::result::Result<(), String> {
    if children.is_empty() {
        return Err("`children` is empty".into());
    }
    for (i, child) in children.iter().enumerate() {
        if child.title.trim().is_empty() {
            return Err(format!("child {i} has an empty title"));
        }
        if !matches!(child.kind.as_str(), "ready" | "plan" | "human") {
            return Err(format!(
                "child {i} has unknown kind `{}` (must be \"ready\", \"plan\" or \"human\")",
                child.kind
            ));
        }
        if let Some(pid) = &child.project
            && !allowed_projects.contains(&pid.as_str())
        {
            return Err(format!(
                "child {i} targets project `{pid}`, which is not the parent's \
                 project or a workspace sibling ({})",
                allowed_projects.join(", ")
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

/// An explicit `spec_depth:` hint in the issue body (ADR 0010). Today the
/// only source is a human writing it into the issue; when triage v1 (#87)
/// lands, its proposal comment / hidden marker feeds the same line. The
/// planner still decides in-context — this only surfaces the hint into the
/// prompt so the agent can honor it. Matching is case-insensitive and
/// tolerates an optional space after the colon.
fn spec_depth_hint(issue_body: &str) -> Option<&'static str> {
    let lower = issue_body.to_lowercase();
    let has = |value: &str| {
        lower.contains(&format!("spec_depth: {value}"))
            || lower.contains(&format!("spec_depth:{value}"))
    };
    if has("design") {
        Some("design")
    } else if has("normal") {
        Some("normal")
    } else {
        None
    }
}

/// Prompt section that makes the spec's depth adaptive (issue #133, ADR
/// 0010): the planner picks `normal` (the default light spec) or `design`
/// (a deeper spec with extra required sections) by uncertainty × blast
/// radius, with a veto that forces migration / rollback whenever persistent
/// state or a public contract is involved. Kept prompt-only — no depth is
/// computed in code — so the decision stays the agent's in-context judgment.
/// An explicit `spec_depth:` hint in the body is surfaced verbatim so the
/// agent honors it (never below the veto floor).
fn adaptive_depth_instruction(issue_body: &str) -> String {
    let hint = match spec_depth_hint(issue_body) {
        Some(depth) => format!(
            "\nThis issue carries an explicit `spec_depth: {depth}` hint — apply \
             it as described above (a `design` hint raises the floor; a `normal` \
             hint never lowers it below what the veto rule demands).\n"
        ),
        None => String::new(),
    };
    format!(
        "# Spec depth — adaptive (normal vs. design)\n\
         Not every issue needs the same spec. Choose the depth by \
         **uncertainty × blast radius**, NOT by implementation effort (you are \
         weak at effort estimates but strong at listing what is undecided and \
         how far a mistake would spread). First enumerate: what is still \
         undecided, and what breaks if you get it wrong. Then pick:\n\
         - **normal spec** (the default described above): acceptance criteria, \
           files to touch, key decisions. For local, well-understood changes.\n\
         - **design spec** (deeper): a normal spec PLUS these required sections \
           — architecture impact / alternatives considered & the decision / \
           migration & rollback (when persistent state is affected) / \
           observability / test strategy. For high uncertainty or wide blast \
           radius.\n\
         **Veto rule (a hard floor that overrides the overall judgment):** if \
         the change touches persistent state, a schema, or a public contract, \
         OR carries an irreversible operational risk, the migration & rollback \
         sections are MANDATORY even if the change otherwise looks small.\n\
         **Record the reason:** state in 1–2 sentences (in the spec body or the \
         PR description) why you chose this depth.\n\
         A design spec is still disposable scaffolding, not a permanent design \
         document: at implementation its architecture / decision content is \
         routed to an ADR, durable domain rules to a domain document, and the \
         rest is distilled into the code — the spec itself is still deleted. \
         The deeper tier is no exception to the disposal rule above.{hint}\n\n"
    )
}

/// Prompt section inviting the decompose ending — except on issues that are
/// themselves decomposition children, where only one level is allowed and
/// the agent is told to hand a still-too-big issue to a human instead. When
/// the project belongs to a workspace, the cross-repo scope (which sibling
/// repos a child may target) is spelled out from config (issue #154).
fn decompose_instruction(deps: &Deps, issue_body: &str) -> String {
    if is_decomposed_child(issue_body) {
        return "# Too big for one spec?\n\
                This issue was itself split out of a bigger issue by a previous \
                decomposition, and decomposition is one level only — do NOT \
                propose another split. If the issue is still too big for one \
                spec, end the turn with `\"status\": \"needs_human\"` and explain \
                why in `summary`.\n\n"
            .to_string();
    }
    format!(
        "# Too big for one spec?\n\
     If your investigation shows the issue cannot converge on one spec and \
     must be split into sub-issues, do NOT write a spec and do NOT create \
     any issues yourself. Instead end the turn with `\"status\": \"decompose\"` \
     in the result file (accepted here in addition to the completion \
     contract's statuses), with:\n\
     - `summary`: one paragraph on why you split it this way (it becomes a \
       comment on the issue).\n\
     - `children`: the sub-issues to file, in dependency order. Each entry is \
       {{\"title\": \"...\", \"body\": \"<minimal body>\", \"kind\": \
       \"ready\"|\"plan\"|\"human\", \"blocked_by\": [<zero-based indices of \
       earlier entries it depends on>]{project_field}}}. \
       Use kind \"ready\" for children small enough to implement directly, \
       \"plan\" for children that still need their own design pass, and \
       \"human\" for a step meguri cannot perform itself (creating a \
       repository, changing visibility, rewriting history, and other \
       irreversible operations) — a `human` child is filed with no trigger \
       label, so meguri never runs it and a person closes it once done, \
       unblocking its dependents.\n\
     {scope}\
     meguri files the sub-issues, wires the `blocked_by` dependencies, and \
     labels them. Decomposition is one level only: sub-issues cannot be \
     decomposed again.\n\n",
        project_field = if deps.config.workspace_of(&deps.project.id).is_some() {
            ", \"project\": \"<sibling project id, optional>\""
        } else {
            ""
        },
        scope = decompose_scope_clause(deps),
    )
}

/// The cross-repo scope paragraph injected into the decompose instruction:
/// present only when the project belongs to a workspace, listing the sibling
/// project ids a child may target. Absent (empty) otherwise, so a project with
/// no workspace sees exactly the single-repo instruction as before.
fn decompose_scope_clause(deps: &Deps) -> String {
    let siblings = deps.config.workspace_siblings(&deps.project.id);
    if siblings.is_empty() {
        return String::new();
    }
    let ws = deps
        .config
        .workspace_of(&deps.project.id)
        .map(|w| w.id.as_str())
        .unwrap_or_default();
    let ids = siblings
        .iter()
        .map(|p| format!("`{}`", p.id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "     - Cross-repo scope: this issue's project (`{project}`) is part of \
       workspace `{ws}`. A child may be filed into a sibling repository by \
       setting `\"project\"` to one of: {ids}. Omit `project` (the default) to \
       file the child in this same repository. The parent (tracking) issue \
       always stays in its own repository — only children may target siblings. \
       Do NOT target any repository outside this list.\n",
        project = deps.project.id,
    )
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

        let cp = Checkpoint::default();
        let err = PlannerFlavor
            .verify_work(&run, &cp, dir.path())
            .unwrap_err();
        assert!(err.contains("docs/specs/issue-7.md"), "{err}");

        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(dir.path().join("docs/specs/issue-7.md"), "# Spec\n").unwrap();
        assert!(PlannerFlavor.verify_work(&run, &cp, dir.path()).is_ok());
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
    fn prompt_routes_durable_value_out_of_the_disposable_spec() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint::default();
        let prompt = PlannerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("disposable scaffolding"));
        assert!(prompt.contains("deleted when the implementation lands"));
        assert!(prompt.contains("docs/adr/NNNN-<slug>.md"));
        assert!(prompt.contains("domain document"));
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
    fn prompt_makes_spec_depth_adaptive_with_veto_and_design_sections() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint::default();
        let prompt = PlannerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        // Decision principle: uncertainty × blast radius, not effort.
        assert!(prompt.contains("Spec depth"));
        assert!(prompt.contains("uncertainty × blast radius"));
        assert!(prompt.contains("NOT by implementation effort"));
        // Two tiers and the veto floor.
        assert!(prompt.contains("normal spec"));
        assert!(prompt.contains("design spec"));
        assert!(prompt.contains("Veto rule"));
        assert!(prompt.contains("migration & rollback"));
        // Design-spec required sections.
        assert!(prompt.contains("architecture impact"));
        assert!(prompt.contains("observability"));
        assert!(prompt.contains("test strategy"));
        // Rationale requirement and disposable-even-when-deep reminder.
        assert!(prompt.contains("why you chose this depth"));
        assert!(prompt.contains("still disposable scaffolding"));
    }

    #[test]
    fn adaptive_depth_surfaces_an_explicit_hint_only_when_present() {
        // No hint: the section is generic, no callout line.
        let plain = adaptive_depth_instruction("Cache the thing.");
        assert!(plain.contains("Spec depth"));
        assert!(!plain.contains("explicit `spec_depth:"));

        // Explicit design hint is surfaced verbatim and framed as a floor.
        let design = adaptive_depth_instruction("Please handle this.\n\nspec_depth: design\n");
        assert!(design.contains("explicit `spec_depth: design` hint"));
        assert!(design.contains("raises the floor"));

        // Explicit normal hint is surfaced but framed as never lowering the veto.
        let normal = adaptive_depth_instruction("spec_depth: normal");
        assert!(normal.contains("explicit `spec_depth: normal` hint"));
        assert!(normal.contains("never lowers it below"));
    }

    #[test]
    fn spec_depth_hint_parses_case_insensitively_and_tolerates_spacing() {
        assert_eq!(spec_depth_hint("nothing here"), None);
        assert_eq!(spec_depth_hint("spec_depth: design"), Some("design"));
        assert_eq!(spec_depth_hint("spec_depth:design"), Some("design"));
        assert_eq!(spec_depth_hint("SPEC_DEPTH: Design"), Some("design"));
        assert_eq!(spec_depth_hint("spec_depth: normal"), Some("normal"));
        // design wins if both somehow appear (raise, never lower).
        assert_eq!(
            spec_depth_hint("spec_depth: normal and spec_depth: design"),
            Some("design")
        );
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
            project: None,
        };
        let allowed = ["proj"];
        assert!(
            validate_children(&[], &allowed)
                .unwrap_err()
                .contains("empty")
        );
        assert!(
            validate_children(&[child("  ", "ready", vec![])], &allowed)
                .unwrap_err()
                .contains("empty title")
        );
        assert!(
            validate_children(&[child("a", "huge", vec![])], &allowed)
                .unwrap_err()
                .contains("unknown kind")
        );
        // Dependencies may only point backwards (no self/forward references).
        assert!(
            validate_children(&[child("a", "ready", vec![0])], &allowed)
                .unwrap_err()
                .contains("earlier")
        );
        // "human" is a valid kind (the human-node ending, issue #154).
        assert!(validate_children(&[child("a", "human", vec![])], &allowed).is_ok());
        assert!(
            validate_children(
                &[child("a", "ready", vec![]), child("b", "plan", vec![0])],
                &allowed
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_children_rejects_out_of_scope_project() {
        let child = ChildIssue {
            title: "x".into(),
            body: String::new(),
            kind: "ready".into(),
            blocked_by: vec![],
            project: Some("stranger".into()),
        };
        let err = validate_children(&[child], &["proj", "sibling"]).unwrap_err();
        assert!(
            err.contains("stranger") && err.contains("workspace sibling"),
            "{err}"
        );

        let ok = ChildIssue {
            title: "x".into(),
            body: String::new(),
            kind: "ready".into(),
            blocked_by: vec![],
            project: Some("sibling".into()),
        };
        assert!(validate_children(&[ok], &["proj", "sibling"]).is_ok());
    }

    #[test]
    fn pr_title_falls_back_to_issue_title_without_subject() {
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        assert_eq!(PlannerFlavor.pr_title(&run, &cp), "Add caching (#7)");
    }

    #[test]
    fn pr_title_prefers_agent_authored_subject_over_spec_prefix_hack() {
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            subject: Some("Write a spec for cache invalidation".into()),
            ..Default::default()
        };
        assert_eq!(
            PlannerFlavor.pr_title(&run, &cp),
            "Write a spec for cache invalidation (#7)"
        );
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
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: None,
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            autonomy: None,
            prompts: Default::default(),
        };
        Deps::with_label_source(
            crate::store::Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(crate::forge::fake::FakeForge::default()),
            crate::config::Config::default(),
            project,
        )
    }
}
