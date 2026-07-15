//! The decomposition materializer sweep (issue #134 / ADR 0016).
//!
//! The planner writes a *decomposition proposal* spec (prose + a machine-readable
//! `children` block) and marks its PR body. Once the spec-review gate approves
//! the PR (`spec-ready` + a per-head `meguri/pr-review` success status), this
//! sweep files the proposed children, wires GitHub-native `blocked_by`, labels
//! each child, and turns the parent into an unlabeled tracking issue — then
//! closes the disposable proposal PR (its single commit point).
//!
//! It is a lightweight forge-only sweep like handoff / reaper — no run record,
//! no pane, no worktree — driven straight from the scheduler poll tick. It is
//! fully re-entrant: while the proposal PR is open every sweep re-runs the whole
//! sequence, so a crash at any point simply gets redone. Duplicate children are
//! prevented by making the parent's dependency graph the authority for "already
//! created?" (strongly consistent, includes closed children), joined by a stable
//! per-child body key, with an all-state marker search covering the narrow
//! create→link window (a reservation marker + defer keeps that window from ever
//! double-creating). A parent-body head pin ties a partially materialized
//! proposal to the approved head it started under, so a re-pushed and
//! re-approved head can never silently adopt the old head's children.

use anyhow::{Context, Result};
use serde_json::json;

use super::Deps;
use super::planner::{self, CHILDREN_FENCE_INFO, decompose_child_footer_ref, decompose_child_key};
use super::pr_reviewer::PR_REVIEW_STATUS;
use crate::forge::{self, CommitStatusState};
use crate::gitops;
use crate::turn::ChildIssue;

/// How many sweeps to keep deferring a reserved-but-unfound child before
/// concluding its create never landed and re-creating it. Bounds the reserve so
/// a crash between the reserve write and `create_issue` cannot deadlock forever,
/// while staying far above GitHub's search-index lag (sweeps are minutes apart,
/// indexing is seconds) so a child that *was* created is found first.
const MAX_DEFER_ATTEMPTS: u32 = 3;

/// The parent-body reservation marker's prefix (without the attempt count) —
/// "an attempt to create child `idx` is in flight". Written *before* the
/// create, so recovery never blindly re-creates a child whose create may have
/// landed but not yet linked.
fn reserve_prefix(idx: usize) -> String {
    format!("<!-- meguri:decompose-reserve idx={idx} attempt=")
}

/// The full reservation marker at a given attempt count.
fn reserve_marker(idx: usize, attempt: u32) -> String {
    format!("{}{attempt} -->", reserve_prefix(idx))
}

/// The recorded attempt count for `idx`'s reservation, if reserved.
fn parse_reserve_attempt(body: &str, idx: usize) -> Option<u32> {
    let prefix = reserve_prefix(idx);
    body.lines().find_map(|l| {
        l.trim()
            .strip_prefix(&prefix)?
            .strip_suffix("-->")?
            .trim()
            .parse::<u32>()
            .ok()
    })
}

/// Hidden ledger line: a human-readable record that child `idx` is filed as
/// `slug#number` (a fast path; the dependency graph is the real authority).
fn ledger_marker(idx: usize, slug: &str, number: i64) -> String {
    format!("<!-- meguri:decompose-ledger idx={idx} issue={slug}#{number} -->")
}

/// The parent-body pin recording which head this parent's materialization ran
/// (or is running) against. The per-child stable key is `parent + idx` only,
/// so without this pin a partially materialized proposal whose head was
/// re-pushed and re-approved would adopt the *old* head's children as the new
/// head's same-index children — silently breaking "the reviewed children block
/// is what gets materialized" (ADR 0016 §5). Written before the first child is
/// created; a mismatch halts the sweep and escalates to a human.
const HEAD_PIN_PREFIX: &str = "<!-- meguri:decompose-head sha=";

/// The full head pin for `sha`.
fn head_pin(sha: &str) -> String {
    format!("{HEAD_PIN_PREFIX}{sha} -->")
}

/// The pinned materialization head recorded in the parent body, if any.
fn parse_head_pin(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        Some(
            l.trim()
                .strip_prefix(HEAD_PIN_PREFIX)?
                .strip_suffix("-->")?
                .trim()
                .to_string(),
        )
    })
}

/// Idempotent error-comment marker so a malformed proposal is flagged on the PR
/// at most once per head sha.
fn error_marker(head_sha: &str) -> String {
    format!("<!-- meguri:decompose-error head={head_sha} -->")
}

/// File the children of every approved, unclosed decomposition-proposal PR.
pub async fn sweep(deps: &Deps) -> Result<()> {
    if deps.forge.is_none() {
        return Ok(()); // local mode has no planner / PRs
    }
    if !deps.config.decompose.materialize_enabled {
        return Ok(()); // kill switch (ADR 0016 rollback lever)
    }
    for pr in deps
        .forge()
        .list_prs_with_label(forge::LABEL_SPEC_READY)
        .await?
    {
        // Only open, marked proposal PRs whose branch encodes an issue.
        if pr.state != "open" || !planner::is_decompose_proposal(&pr.body) {
            continue;
        }
        // Respect the stop / ball labels like every other PR loop: a human
        // pausing (`hold`) or escalating (`needs-human`) an approved proposal,
        // or another run claiming it (`working`), must halt the irreversible
        // child-creation. `hold` in particular is the operator's brake.
        if pr.has_label(forge::LABEL_HOLD)
            || pr.has_label(forge::LABEL_WORKING)
            || pr.has_label(forge::LABEL_NEEDS_HUMAN)
        {
            continue;
        }
        let Some(parent) = gitops::issue_from_branch(&pr.head_branch) else {
            continue; // human-made head: not meguri's to materialize
        };
        if let Err(e) = process(deps, &pr, parent).await {
            tracing::warn!(
                "decompose materialize sweep failed for {} #{parent}: {e:#}",
                deps.project.id
            );
        }
    }
    Ok(())
}

/// One approved proposal PR, end to end. Returns `Ok` after doing as much as is
/// safe this sweep; anything unfinished is retried next tick (the proposal PR
/// stays open until the single commit point — closing it — succeeds).
async fn process(deps: &Deps, pr: &forge::PullRequest, parent: i64) -> Result<()> {
    // Stop / ball labels on the *parent issue* halt materialization too: the
    // issue's labels are the canonical phase/ball record (2-axis model, ADR
    // 0005), so a human pausing (`hold`) or escalating (`needs-human`) the
    // parent must stop the irreversible child creation even when the proposal
    // PR itself carries no such label. Checked before the head gate so a held
    // parent is left completely untouched (no spec-ready/reviewing churn).
    let parent_issue = deps.forge().get_issue(parent).await?;
    if parent_issue.has_label(forge::LABEL_HOLD) || parent_issue.has_label(forge::LABEL_NEEDS_HUMAN)
    {
        return Ok(());
    }

    // Head-motion gate: only materialize the head the pr-reviewer actually reviewed
    // (ADR 0016 §5). The approval trail is the per-head pr-review status.
    let guard_on = deps.config.review_for(&deps.project).guard.plan;
    if guard_on {
        let approved = deps
            .forge()
            .commit_status(&pr.head_sha, PR_REVIEW_STATUS)
            .await?
            == Some(CommitStatusState::Success);
        if !approved {
            // The reviewed head moved on. Send it back to review — the guard's
            // existing discover (spec-reviewing + unreviewed head) re-reviews it,
            // and a clean verdict returns it to spec-ready for a later sweep. No
            // dedicated driver, no new marker.
            deps.forge()
                .add_pr_label(pr.number, forge::LABEL_SPEC_REVIEWING)
                .await?;
            deps.forge()
                .remove_pr_label(pr.number, forge::LABEL_SPEC_READY)
                .await
                .ok();
            deps.store.emit(
                None,
                "issue.materialize_head_stale",
                json!({ "parent": parent, "pr": pr.number, "head": pr.head_sha }),
            )?;
            return Ok(());
        }
    }

    let parent_slug =
        deps.project.repo_slug.clone().context(
            "project has no repo_slug, so a decomposition proposal cannot be materialized",
        )?;

    // Read the proposal spec from the exact approved head (not the working
    // tree). Fetch first; if the branch tip moved under us mid-sweep, skip and
    // let the next tick re-evaluate the new head.
    let repo_path = deps.repo_path();
    let tip = gitops::fetch_branch_tip(&repo_path, &pr.head_branch).await?;
    if tip != pr.head_sha {
        return Ok(()); // head moved during the sweep; re-evaluate next tick
    }
    let spec_path = planner::spec_rel_path(parent);
    let spec = gitops::show_file_at_ref(&repo_path, &pr.head_sha, &spec_path).await?;

    materialize(deps, pr, parent, &parent_slug, &spec).await
}

/// The git-free core: parse the reviewed spec, adopt-or-create each child
/// idempotently, and finalize (parent tracking + close the proposal PR). Split
/// out so it is unit-testable against `FakeForge` without a real git repo.
async fn materialize(
    deps: &Deps,
    pr: &forge::PullRequest,
    parent: i64,
    parent_slug: &str,
    spec: &str,
) -> Result<()> {
    let parent_ref = format!("{parent_slug}#{parent}");

    // Parse + validate the reviewed children block. Any problem stops before a
    // single issue is created and is surfaced on the PR once per head.
    let children = match parse_children_block(spec) {
        Ok(children) => children,
        Err(problem) => {
            return report_error(deps, pr, &problem).await;
        }
    };
    let allowed = allowed_projects(deps);
    let allowed_refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
    if let Err(problem) = planner::validate_children(&children, &allowed_refs) {
        return report_error(deps, pr, &format!("children block is invalid: {problem}")).await;
    }

    // Head pin (cross-sweep half of the head-motion gate, ADR 0016 §5): the
    // per-sweep guard-status check proves the *current* head was reviewed, but
    // a materialization that started under an earlier approved head may have
    // already created children keyed only by `parent + idx`. Adopting those as
    // this head's children would materialize a mix of two reviewed specs, so a
    // pinned head that differs stops everything and hands the parent to a
    // human (children are irreversible; no automatic reconciliation).
    let parent_body = deps.forge().get_issue(parent).await?.body;
    match parse_head_pin(&parent_body) {
        None => append_parent_marker(deps, parent, &head_pin(&pr.head_sha)).await?,
        Some(pinned) if pinned == pr.head_sha => {} // resuming the same head
        Some(pinned) => return report_head_conflict(deps, pr, parent, &pinned).await,
    }

    // Adopt-or-create each child in dependency order, idempotently.
    let mut filed: Vec<Filed> = Vec::with_capacity(children.len());
    for (idx, child) in children.iter().enumerate() {
        match adopt_or_create(deps, parent, parent_slug, &parent_ref, idx, child, &filed).await? {
            Some(f) => filed.push(f),
            None => {
                // Deferred: a reserved child whose create may not have landed
                // yet. Never re-create (duplicate issues are irreversible);
                // retry next sweep once the graph / search catches up.
                deps.store.emit(
                    None,
                    "issue.materialize_deferred",
                    json!({ "parent": parent, "pr": pr.number, "idx": idx }),
                )?;
                return Ok(());
            }
        }
    }

    finalize(deps, pr, parent, &children, &filed).await
}

/// A child that has been filed (created or adopted this sweep or earlier).
struct Filed {
    number: i64,
    slug: String,
    /// The forge of the child's home repo (its own repo, or a workspace sibling
    /// for a cross-repo child) — where finalize applies its phase label.
    forge: std::sync::Arc<dyn forge::Forge>,
}

/// The projects a child may target: the parent's own project plus its workspace
/// siblings (issue #154 / ADR 0009).
fn allowed_projects(deps: &Deps) -> Vec<String> {
    let mut ids = vec![deps.project.id.clone()];
    ids.extend(
        deps.config
            .workspace_siblings(&deps.project.id)
            .iter()
            .map(|p| p.id.clone()),
    );
    ids
}

/// Resolve or create child `idx`, then (idempotently) wire its dependencies,
/// labels, and ledger. `Ok(None)` means "deferred — retry next sweep".
async fn adopt_or_create(
    deps: &Deps,
    parent: i64,
    parent_slug: &str,
    parent_ref: &str,
    idx: usize,
    child: &ChildIssue,
    filed: &[Filed],
) -> Result<Option<Filed>> {
    let key = decompose_child_key(parent_ref, idx);
    let (child_forge, child_slug) = planner::resolve_child_target(deps, parent_slug, child)?;

    // 1. Authority: is idx already a blocker of the parent (graph)? Match the
    //    per-child key in the blocker's body. Includes closed children.
    let (number, slug) = if let Some(existing) = find_in_graph(deps, parent, &key).await? {
        deps.store.emit(
            None,
            "issue.materialize_resumed",
            json!({ "parent": parent, "idx": idx, "via": "graph", "child": existing.0 }),
        )?;
        existing
    } else {
        // 2. Not linked in the parent graph yet.
        let parent_body = deps.forge().get_issue(parent).await?.body;
        match parse_reserve_attempt(&parent_body, idx) {
            // First encounter: reserve *before* creating, so a crash between the
            // create and the link is not blindly re-created next time.
            None => {
                set_reserve(deps, parent, idx, 0).await?;
                let number = create_child(
                    deps,
                    &child_forge,
                    parent,
                    parent_slug,
                    &child_slug,
                    child,
                    &key,
                )
                .await?;
                (number, child_slug.clone())
            }
            // 3. Reserved but not in the graph: the create may have landed and
            //    not linked, or may never have landed. Backstop: all-state key
            //    search on the child's own repo.
            Some(attempt) => match child_forge.find_issue_by_marker(&key).await? {
                Some(number) => {
                    deps.forge()
                        .add_blocked_by_in(parent, &child_slug, number)
                        .await?;
                    deps.store.emit(
                        None,
                        "issue.materialize_resumed",
                        json!({ "parent": parent, "idx": idx, "via": "search", "child": number }),
                    )?;
                    (number, child_slug.clone())
                }
                // Neither graph nor search sees it. Wait one more sweep (search
                // lag) rather than risk a duplicate — but only up to the bound.
                None if attempt + 1 < MAX_DEFER_ATTEMPTS => {
                    set_reserve(deps, parent, idx, attempt + 1).await?;
                    return Ok(None); // defer; the caller records it
                }
                // Waited long past the search-index lag: the create never
                // landed. Safe to re-create — no living child exists.
                None => {
                    let number = create_child(
                        deps,
                        &child_forge,
                        parent,
                        parent_slug,
                        &child_slug,
                        child,
                        &key,
                    )
                    .await?;
                    (number, child_slug.clone())
                }
            },
        }
    };

    // Idempotent wiring for both created and adopted children: sibling deps and
    // the ledger. The phase label is deliberately NOT applied here — it is
    // applied in finalize, once every child's blockers are wired, so a partial
    // run never leaves a labeled-but-unblocked (prematurely discoverable) child.
    for &dep in &child.blocked_by {
        let blocker = &filed[dep];
        child_forge
            .add_blocked_by_in(number, &blocker.slug, blocker.number)
            .await?;
    }
    append_parent_marker(deps, parent, &ledger_marker(idx, &slug, number)).await?;

    Ok(Some(Filed {
        number,
        slug,
        forge: child_forge,
    }))
}

/// Create child `child` in its target repo with the stable key + parent footer
/// in its body, then link it into the parent's dependency graph. The key is
/// written atomically with the create, so "created" and "findable" are
/// inseparable.
///
/// The child is created **unlabeled**: its phase label (`meguri:ready` /
/// `meguri:plan`) is applied only in [`finalize`], after every child exists and
/// every `blocked_by` edge is wired. Labeling at create time would make a child
/// discoverable by the worker/planner (which run before this sweep on the next
/// tick) before its own blockers were set — a crash between create and the
/// sibling `blocked_by` add could let a blocked child start out of order.
async fn create_child(
    deps: &Deps,
    child_forge: &std::sync::Arc<dyn forge::Forge>,
    parent: i64,
    parent_slug: &str,
    child_slug: &str,
    child: &ChildIssue,
    key: &str,
) -> Result<i64> {
    let body = format!(
        "{}{}\n{key}",
        child.body.trim(),
        decompose_child_footer_ref(&child_parent_ref(parent, parent_slug, child_slug)),
    );
    let number = child_forge.create_issue(&child.title, &body, &[]).await?;
    // Cross-repo children need the slug-qualified add (issue #154): the parent
    // graph must include the sibling child so a resume can adopt it.
    deps.forge()
        .add_blocked_by_in(parent, child_slug, number)
        .await?;
    Ok(number)
}

/// Upsert the reservation marker for `idx` at `attempt` in the parent body
/// (replace the existing reserve line, else append it). Idempotent per attempt.
async fn set_reserve(deps: &Deps, parent: i64, idx: usize, attempt: u32) -> Result<()> {
    let body = deps.forge().get_issue(parent).await?.body;
    let prefix = reserve_prefix(idx);
    let marker = reserve_marker(idx, attempt);
    let mut replaced = false;
    let mut lines: Vec<String> = Vec::new();
    for l in body.lines() {
        if l.trim().starts_with(&prefix) {
            lines.push(marker.clone());
            replaced = true;
        } else {
            lines.push(l.to_string());
        }
    }
    let new_body = if replaced {
        lines.join("\n")
    } else {
        format!("{}\n{marker}", body.trim_end())
    };
    deps.forge().update_issue_body(parent, &new_body).await
}

/// The slug-aware parent reference for a child's human-visible footer — `#N`
/// within the parent's repo, `owner/repo#N` for a cross-repo child (issue #154).
fn child_parent_ref(parent: i64, parent_slug: &str, child_slug: &str) -> String {
    if child_slug == parent_slug {
        format!("#{parent}")
    } else {
        format!("{parent_slug}#{parent}")
    }
}

/// Find a blocker of `parent` whose body carries `key` (the dependency graph is
/// strongly consistent and returns closed children too). Returns `(number, slug)`.
async fn find_in_graph(deps: &Deps, parent: i64, key: &str) -> Result<Option<(i64, String)>> {
    let parent_slug = deps.project.repo_slug.clone().unwrap_or_default();
    Ok(deps
        .forge()
        .blocked_by(parent)
        .await?
        .into_iter()
        .find(|b| b.body.contains(key))
        .map(|b| {
            // A same-repo blocker may come back with an empty repo (the forge
            // did not qualify it); treat empty as the parent's own repo.
            let slug = if b.repo.is_empty() {
                parent_slug.clone()
            } else {
                b.repo.clone()
            };
            (b.number, slug)
        }))
}

/// Append a hidden marker line to the parent body, unless already present
/// (idempotent). The parent body is the crash-recovery ledger.
async fn append_parent_marker(deps: &Deps, parent: i64, marker: &str) -> Result<()> {
    let body = deps.forge().get_issue(parent).await?.body;
    if body.contains(marker) {
        return Ok(());
    }
    let new_body = format!("{body}\n{marker}");
    deps.forge().update_issue_body(parent, &new_body).await
}

/// Turn the parent into a tracking issue and close the proposal PR — the single
/// commit point. All steps are idempotent, so this re-runs cleanly until the
/// close lands and the PR drops out of discovery.
async fn finalize(
    deps: &Deps,
    pr: &forge::PullRequest,
    parent: i64,
    children: &[ChildIssue],
    filed: &[Filed],
) -> Result<()> {
    // The parent visibly waits on every child (parent→child edges are also set
    // per child in adopt_or_create; re-adding is a no-op).
    for f in filed {
        deps.forge()
            .add_blocked_by_in(parent, &f.slug, f.number)
            .await?;
    }

    // Apply each child's phase label now — only here, after every child exists
    // and every blocked_by edge is wired. This is what makes a child
    // discoverable by the worker/planner, so a blocked child never becomes
    // actionable before its blocker edge is in place (idempotent; `human`
    // children carry no trigger label). A crash mid-labeling is safe: any child
    // already labeled already has its blockers, so discovery gates it correctly.
    let own_slug = deps.project.repo_slug.as_deref();
    for (child, f) in children.iter().zip(filed) {
        if let Some(label) = planner::child_label(child) {
            f.forge.add_label(f.number, label).await?;
            // Watched-label notify (issue #205): children are filed label-less
            // for safety, so the label lands here, not at create_issue time —
            // this is the notify hook for the materialize path. Own-repo only:
            // a sibling's child is governed by the sibling project's config.
            if own_slug == Some(f.slug.as_str()) {
                deps.notify_created_issue(f.number, &child.title, &[label])
                    .await;
            }
        }
    }

    // Tracking checklist (upserted so re-runs replace, not duplicate).
    let listing = children
        .iter()
        .zip(filed)
        .map(|(child, f)| {
            format!(
                "- [ ] {}#{} (`{}`) {}",
                f.slug, f.number, child.kind, child.title
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let parent_body = deps.forge().get_issue(parent).await?.body;
    let tracking = format!(
        "{START}\n### 子 issue (tracking)\n\n{listing}\n{END}",
        START = TRACKING_START,
        END = TRACKING_END,
    );
    deps.forge()
        .update_issue_body(parent, &upsert_block(&parent_body, &tracking))
        .await?;

    // The parent becomes an unlabeled tracking issue (2-axis model, ADR 0005):
    // drop its phase / ball labels. Idempotent.
    for label in [
        forge::LABEL_PLAN,
        forge::LABEL_SPECCING,
        forge::LABEL_READY,
        forge::LABEL_WORKING,
    ] {
        deps.forge().remove_label(parent, label).await.ok();
    }

    // Human-visible rationale comment, once. There is no issue-comment reader,
    // so dedup on a hidden marker in the parent body (which we already read).
    let commented_marker = "<!-- meguri:decompose-commented -->";
    let parent_body = deps.forge().get_issue(parent).await?.body;
    if !parent_body.contains(commented_marker) {
        let refs = filed
            .iter()
            .zip(children)
            .map(|(f, child)| {
                format!(
                    "- {}#{} (`{}`) {}",
                    f.slug, f.number, child.kind, child.title
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        deps.forge()
            .comment(
                parent,
                &format!(
                    "🧩 **meguri**: 分解提案が承認されたので、この issue を tracking issue にして \
                     子 issue を起こしました:\n\n{refs}\n\n順序は `blocked_by` で制御します。\
                     子が全部 close されたら、この親を閉じてください。"
                ),
            )
            .await
            .ok();
        append_parent_marker(deps, parent, commented_marker).await?;
    }

    // Commit point: close the disposable proposal PR unmerged. Only after this
    // does the sweep stop re-processing (state != open).
    deps.forge().close_pr(pr.number).await?;
    deps.store.emit(
        None,
        "issue.materialized",
        json!({ "parent": parent, "children": filed.iter().map(|f| f.number).collect::<Vec<_>>() }),
    )?;
    Ok(())
}

const TRACKING_START: &str = "<!-- meguri:decompose-tracking:start -->";
const TRACKING_END: &str = "<!-- meguri:decompose-tracking:end -->";

/// Replace the tracking block if present, else append it — so finalize is
/// idempotent on the parent body.
fn upsert_block(body: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (body.find(TRACKING_START), body.find(TRACKING_END)) {
        let end = end + TRACKING_END.len();
        format!("{}{}{}", &body[..start], block, &body[end..])
    } else {
        format!("{}\n\n{block}", body.trim_end())
    }
}

/// A materialization that began under a different (previously approved) head:
/// halt before creating anything, put the ball on the parent issue
/// (`meguri:needs-human` — the issue's labels are the canonical ball, so the
/// parent-label stop gate keeps every later sweep away), and explain once per
/// head on the proposal PR. A human resolves it by either restoring the pinned
/// head (and re-approving it) or cleaning up the old head's children and the
/// parent's `meguri:decompose-*` markers before removing the label.
async fn report_head_conflict(
    deps: &Deps,
    pr: &forge::PullRequest,
    parent: i64,
    pinned: &str,
) -> Result<()> {
    deps.forge()
        .add_label(parent, forge::LABEL_NEEDS_HUMAN)
        .await?;
    let marker = format!(
        "<!-- meguri:decompose-head-conflict pinned={pinned} head={} -->",
        pr.head_sha
    );
    let existing = deps
        .forge()
        .pr_comments(pr.number)
        .await
        .unwrap_or_default();
    if !existing.iter().any(|c| c.contains(&marker)) {
        deps.forge()
            .comment_pr(
                pr.number,
                &format!(
                    "⚠️ **meguri**: この分解提案は head `{pinned}` で materialize を開始済みですが、\
                     承認された head が `{}` に変わっています。旧 head 起点の子 issue を新 head の\
                     子として採用すると「レビューされた children = 実体化される children」が壊れる\
                     ため、停止して親 issue に `meguri:needs-human` を付けました。\n\n\
                     対処: (a) 旧 head の切り方で続行するなら head を `{pinned}` に戻して再承認する、\
                     (b) 新 head で進めるなら旧 head 起点の子 issue と親 body の \
                     `meguri:decompose-*` マーカーを整理する — その上で親の `meguri:needs-human` を\
                     外してください。\n\n{marker}",
                    pr.head_sha
                ),
            )
            .await
            .ok();
    }
    deps.store.emit(
        None,
        "issue.materialize_head_conflict",
        json!({ "parent": parent, "pr": pr.number, "pinned": pinned, "head": pr.head_sha }),
    )?;
    tracing::warn!(
        "decompose proposal PR #{} approved head {} conflicts with pinned materialization head {pinned}; escalated to human",
        pr.number,
        pr.head_sha
    );
    Ok(())
}

/// Post a one-shot error comment on a malformed proposal (no issue is created).
async fn report_error(deps: &Deps, pr: &forge::PullRequest, problem: &str) -> Result<()> {
    let marker = error_marker(&pr.head_sha);
    let existing = deps
        .forge()
        .pr_comments(pr.number)
        .await
        .unwrap_or_default();
    if existing.iter().any(|c| c.contains(&marker)) {
        return Ok(());
    }
    deps.forge()
        .comment_pr(
            pr.number,
            &format!(
                "⚠️ **meguri**: この分解提案は materialize できません。子 issue は1つも \
                 作っていません。spec を直して push してください。\n\n> {problem}\n\n{marker}"
            ),
        )
        .await
        .ok();
    tracing::warn!(
        "decompose proposal PR #{} is malformed: {problem}",
        pr.number
    );
    Ok(())
}

/// Extract the one `children` array from the proposal spec's unique fenced
/// block (info string `json meguri-children`). Exactly one block is valid;
/// zero, many, or unparseable is an error (never a silent guess).
pub fn parse_children_block(spec: &str) -> std::result::Result<Vec<ChildIssue>, String> {
    let mut blocks = Vec::new();
    let mut lines = spec.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let Some(info) = trimmed.strip_prefix("```") else {
            continue;
        };
        if !info.trim().contains(CHILDREN_FENCE_INFO) {
            continue;
        }
        // Collect until the closing fence.
        let mut body = String::new();
        let mut closed = false;
        for l in lines.by_ref() {
            if l.trim_start().starts_with("```") {
                closed = true;
                break;
            }
            body.push_str(l);
            body.push('\n');
        }
        if !closed {
            return Err("children block fence is not closed".into());
        }
        blocks.push(body);
    }
    match blocks.len() {
        0 => Err(format!(
            "no `{CHILDREN_FENCE_INFO}` block found in the proposal spec"
        )),
        1 => serde_json::from_str::<Vec<ChildIssue>>(&blocks[0])
            .map_err(|e| format!("children block is not a valid JSON array: {e}")),
        n => Err(format!(
            "found {n} `{CHILDREN_FENCE_INFO}` blocks; there must be exactly one"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::Forge;
    use crate::forge::fake::FakeForge;
    use std::sync::Arc;

    const HEAD: &str = "sha-approved";

    fn spec_with(children_json: &str) -> String {
        format!(
            "# 分解提案\n\nprose about coverage.\n\n```{CHILDREN_FENCE_INFO}\n{children_json}\n```\n\ntail.\n"
        )
    }

    fn two_children() -> String {
        spec_with(
            r#"[
              {"title":"Child A","body":"do a","kind":"ready","blocked_by":[]},
              {"title":"Child B","body":"do b","kind":"plan","blocked_by":[0]}
            ]"#,
        )
    }

    fn project() -> crate::config::ProjectConfig {
        crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: None,
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
        }
    }

    /// Deps over a FakeForge with a seeded parent issue (#1, `speccing`) and a
    /// marked, spec-ready proposal PR (#10) on branch `meguri/1-…` at HEAD.
    fn setup() -> (Arc<FakeForge>, Deps) {
        let forge = Arc::new(FakeForge::default());
        forge.issues.lock().unwrap().push(crate::forge::Issue {
            number: 1,
            title: "Parent".into(),
            body: String::new(),
            labels: vec![crate::forge::LABEL_SPECCING.into()],
        });
        forge.add_pr(
            10,
            "Parent",
            &format!("body\n{}", planner::DECOMPOSE_PROPOSAL_MARKER),
            &[crate::forge::LABEL_SPEC_READY],
            "meguri/1-parent-abc",
            HEAD,
        );
        let deps = Deps::with_label_source(
            crate::store::Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            crate::config::Config::default(),
            project(),
        );
        (forge, deps)
    }

    async fn pr_of(forge: &FakeForge) -> forge::PullRequest {
        forge.get_pr(10).await.unwrap()
    }

    #[tokio::test]
    async fn materialize_files_children_wires_deps_labels_and_closes_pr() {
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();

        // Parent (#1) + two children (#2, #3).
        assert_eq!(forge.all_issues().len(), 3);
        // Phase labels: child A ready, child B plan.
        assert!(
            forge
                .labels_of(2)
                .contains(&crate::forge::LABEL_READY.to_string())
        );
        assert!(
            forge
                .labels_of(3)
                .contains(&crate::forge::LABEL_PLAN.to_string())
        );
        // Sibling dependency: B blocked by A.
        assert_eq!(forge.blockers_of(3), vec![2]);
        // Parent waits on both children.
        let mut parent_blockers = forge.blockers_of(1);
        parent_blockers.sort();
        assert_eq!(parent_blockers, vec![2, 3]);
        // Parent is an unlabeled tracking issue (phase stripped).
        assert!(forge.labels_of(1).is_empty());
        // The proposal PR is closed unmerged.
        assert_eq!(forge.get_pr(10).await.unwrap().state, "closed");
    }

    #[tokio::test]
    async fn materialize_notifies_watched_labels_after_finalize_applies_them() {
        // Children are filed label-less and labeled only in finalize, so the
        // watched-label notify must fire there — not at create_issue (#205).
        let (forge, mut deps) = setup();
        deps.project.notify = Some(crate::config::ProjectNotifyConfig {
            labels: vec![
                crate::forge::LABEL_READY.to_string(),
                crate::forge::LABEL_PLAN.to_string(),
            ],
        });
        let (notifier, gw) = crate::notify::fake::recording_notifier();
        deps.notifier = notifier;
        let pr = pr_of(&forge).await;

        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();

        let mut labels: Vec<_> = gw
            .delivered()
            .into_iter()
            .filter(|n| n.event == "label")
            .map(|n| n.dedup_key)
            .collect();
        labels.sort();
        // Child A (#2, ready) and child B (#3, plan) both notified.
        assert_eq!(labels, vec!["issue:2".to_string(), "issue:3".to_string()]);
    }

    #[tokio::test]
    async fn materialize_is_idempotent_no_duplicate_children() {
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();
        let after_first = forge.all_issues().len();
        // Re-run the whole sequence (re-entrant sweep).
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();
        assert_eq!(
            forge.all_issues().len(),
            after_first,
            "no duplicate children"
        );
    }

    #[tokio::test]
    async fn materialize_adopts_child_already_in_graph_after_crash() {
        // Simulate a crash after child A was created + linked but before B: the
        // parent graph already holds A (with its key), so the resume adopts A
        // and only creates B — never a duplicate A.
        let (forge, deps) = setup();
        let key = decompose_child_key("me/proj#1", 0);
        let child_a = forge
            .create_issue(
                "Child A",
                &format!("do a{}\n{key}", decompose_child_footer_ref("#1")),
                &[crate::forge::LABEL_READY],
            )
            .await
            .unwrap();
        forge.block_issue(1, child_a); // parent → A edge (graph authority)

        let pr = pr_of(&forge).await;
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();

        // Still exactly parent + A + B; A was adopted, not re-created.
        assert_eq!(forge.all_issues().len(), 3);
        assert_eq!(child_a, 2);
        assert_eq!(forge.blockers_of(3), vec![2]); // B blocked by the adopted A
    }

    #[tokio::test]
    async fn materialize_adopts_a_closed_child_from_the_graph() {
        // A child closed by a human before we recorded it is still returned by
        // blocked_by, so graph adoption recognizes it (no re-create).
        let (forge, deps) = setup();
        let key = decompose_child_key("me/proj#1", 0);
        let child_a = forge
            .create_issue(
                "Child A",
                &format!("do a\n{key}"),
                &[crate::forge::LABEL_READY],
            )
            .await
            .unwrap();
        forge.block_issue(1, child_a);
        forge.close_issue(child_a); // closed as completed

        let pr = pr_of(&forge).await;
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();
        assert_eq!(
            forge.all_issues().len(),
            3,
            "closed child adopted, not re-created"
        );
    }

    #[tokio::test]
    async fn malformed_children_block_creates_no_issue_and_comments_once() {
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;
        // Two blocks → ambiguous → error, no issues.
        let bad = format!(
            "{}\n{}",
            spec_with(r#"[{"title":"A","kind":"ready"}]"#),
            spec_with(r#"[{"title":"B","kind":"ready"}]"#),
        );
        materialize(&deps, &pr, 1, "me/proj", &bad).await.unwrap();
        assert_eq!(forge.all_issues().len(), 1, "no child issues created");
        assert_eq!(forge.pr_comments_of(10).len(), 1, "one error comment");
        // Idempotent: same head → no second comment.
        materialize(&deps, &pr, 1, "me/proj", &bad).await.unwrap();
        assert_eq!(forge.pr_comments_of(10).len(), 1);
    }

    #[tokio::test]
    async fn head_stale_flips_back_to_reviewing_and_creates_nothing() {
        // guard.plan on (default) but no pr-review success on the head:
        // the proposal is not materialized and goes back to spec-reviewing.
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;
        process(&deps, &pr, 1).await.unwrap();
        assert_eq!(forge.all_issues().len(), 1, "no children on a stale head");
        let labels = forge.pr_labels_of(10);
        assert!(labels.contains(&crate::forge::LABEL_SPEC_REVIEWING.to_string()));
        assert!(!labels.contains(&crate::forge::LABEL_SPEC_READY.to_string()));
    }

    #[tokio::test]
    async fn guard_off_does_not_apply_the_head_gate() {
        // AC 9: with `guard.plan = false` (approval gate opted out) the sweep
        // does not require a pr-review status and does not flip the PR back
        // to spec-reviewing — the head gate is off with the same switch. (This
        // unit has no git repo, so `process` errors when it goes on to read the
        // spec; the point is only that the gate did not fire.)
        let (forge, _deps) = setup();
        let mut config = crate::config::Config::default();
        config.review.guard.plan = false;
        let deps = Deps::with_label_source(
            crate::store::Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            config,
            project(),
        );
        let pr = pr_of(&forge).await;
        let _ = process(&deps, &pr, 1).await; // errors on the (absent) git read
        let labels = forge.pr_labels_of(10);
        assert!(
            labels.contains(&crate::forge::LABEL_SPEC_READY.to_string()),
            "guard off: head gate did not flip the label"
        );
        assert!(!labels.contains(&crate::forge::LABEL_SPEC_REVIEWING.to_string()));
    }

    fn one_child() -> String {
        spec_with(r#"[{"title":"Only","body":"x","kind":"ready","blocked_by":[]}]"#)
    }

    #[tokio::test]
    async fn sweep_skips_a_held_proposal() {
        // A human `hold` on an approved proposal halts the irreversible
        // child-creation (finding: stop labels).
        let (forge, deps) = setup();
        forge
            .add_pr_label(10, crate::forge::LABEL_HOLD)
            .await
            .unwrap();
        sweep(&deps).await.unwrap();
        assert_eq!(
            forge.all_issues().len(),
            1,
            "held proposal is not materialized"
        );
        assert_eq!(forge.get_pr(10).await.unwrap().state, "open");
    }

    #[tokio::test]
    async fn process_skips_a_parent_with_stop_labels_untouched() {
        // `hold` / `needs-human` on the *parent issue* (the canonical
        // phase/ball record) halt materialization entirely — no children, and
        // no spec-ready → spec-reviewing churn either (finding: parent stop
        // labels). Without the skip, the guard gate would flip the labels
        // because HEAD carries no pr-review status.
        let (forge, deps) = setup();
        for label in [crate::forge::LABEL_HOLD, crate::forge::LABEL_NEEDS_HUMAN] {
            forge.add_label(1, label).await.unwrap();
            let pr = pr_of(&forge).await;
            process(&deps, &pr, 1).await.unwrap();
            assert_eq!(forge.all_issues().len(), 1, "no children under {label}");
            let labels = forge.pr_labels_of(10);
            assert!(
                labels.contains(&crate::forge::LABEL_SPEC_READY.to_string()),
                "spec-ready kept under {label}"
            );
            assert!(
                !labels.contains(&crate::forge::LABEL_SPEC_REVIEWING.to_string()),
                "not flipped to spec-reviewing under {label}"
            );
            forge.remove_label(1, label).await.unwrap();
        }
    }

    #[tokio::test]
    async fn materialize_pins_the_approved_head_on_the_parent() {
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();
        assert_eq!(
            parse_head_pin(&forge.get_issue(1).await.unwrap().body).as_deref(),
            Some(HEAD),
            "the parent records which head was materialized"
        );
    }

    #[tokio::test]
    async fn materialize_halts_and_escalates_when_pinned_to_a_different_head() {
        // A previous sweep began materializing under another approved head:
        // the pin and child A (idx 0, old spec) already exist. The head then
        // moved and got re-approved. Adopting old-head children as the new
        // head's same-index children would break "reviewed children block =
        // what gets materialized", so the sweep must stop and hand the parent
        // to a human (finding: stable key is parent+idx only).
        let (forge, deps) = setup();
        forge
            .update_issue_body(1, &head_pin("sha-old"))
            .await
            .unwrap();
        let key = decompose_child_key("me/proj#1", 0);
        let child_a = forge
            .create_issue(
                "Old Child A",
                &format!("do a\n{key}"),
                &[crate::forge::LABEL_READY],
            )
            .await
            .unwrap();
        forge.block_issue(1, child_a);

        let pr = pr_of(&forge).await; // approved head = HEAD != sha-old
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();

        // Nothing created or adopted; the proposal PR stays open; the ball is
        // on the parent issue, which also keeps later sweeps away.
        assert_eq!(forge.all_issues().len(), 2, "no new children");
        assert_eq!(forge.get_pr(10).await.unwrap().state, "open");
        assert!(
            forge
                .labels_of(1)
                .contains(&crate::forge::LABEL_NEEDS_HUMAN.to_string()),
            "parent escalated to needs-human"
        );
        assert_eq!(forge.pr_comments_of(10).len(), 1, "one conflict comment");

        // Idempotent per head: a re-run adds no second comment.
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();
        assert_eq!(forge.pr_comments_of(10).len(), 1);
        assert_eq!(forge.all_issues().len(), 2);
    }

    #[test]
    fn head_pin_round_trips() {
        let body = format!("intro\n{}\ntail", head_pin("abc123"));
        assert_eq!(parse_head_pin(&body).as_deref(), Some("abc123"));
        assert_eq!(parse_head_pin("no pin here"), None);
    }

    #[tokio::test]
    async fn reserved_but_uncreated_defers_then_recreates_without_deadlock() {
        // Crash after the reserve write but before create_issue: the child does
        // not exist and is not in the graph. Early attempts defer (never risk a
        // duplicate); past the bound the sweep concludes the create never landed
        // and re-creates — no permanent stall (finding: reserve deadlock).
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;

        // Low attempt → defer, no child, reserve bumped.
        forge
            .update_issue_body(1, &reserve_marker(0, 0))
            .await
            .unwrap();
        materialize(&deps, &pr, 1, "me/proj", &one_child())
            .await
            .unwrap();
        assert_eq!(forge.all_issues().len(), 1, "deferred: no child yet");
        assert!(
            parse_reserve_attempt(&forge.get_issue(1).await.unwrap().body, 0).unwrap() >= 1,
            "attempt bumped"
        );

        // At the bound → re-create (the create had truly never landed).
        forge
            .update_issue_body(1, &reserve_marker(0, MAX_DEFER_ATTEMPTS - 1))
            .await
            .unwrap();
        materialize(&deps, &pr, 1, "me/proj", &one_child())
            .await
            .unwrap();
        assert_eq!(forge.all_issues().len(), 2, "re-created after the bound");
        assert_eq!(
            forge.blockers_of(1),
            vec![2],
            "parent now waits on the child"
        );
    }

    #[tokio::test]
    async fn partial_materialize_leaves_created_children_unlabeled() {
        // Crash mid-materialization (here forced via a deferred later child):
        // an already-created child must not carry its phase label yet, or the
        // worker/planner discovery (which runs before this sweep next tick) could
        // start it before its dependency edges settle. Labels land only in
        // finalize, once the whole graph is wired.
        let (forge, deps) = setup();
        let pr = pr_of(&forge).await;
        // Reserve child idx 1 so it defers; idx 0 (A) is created this sweep.
        forge
            .update_issue_body(1, &reserve_marker(1, 0))
            .await
            .unwrap();
        materialize(&deps, &pr, 1, "me/proj", &two_children())
            .await
            .unwrap();

        // A (#2) was created and linked, but carries NO phase label yet.
        assert_eq!(forge.all_issues().len(), 2, "only A created; B deferred");
        assert_eq!(forge.blockers_of(1), vec![2], "parent → A edge wired");
        let a_labels = forge.labels_of(2);
        assert!(
            !a_labels.contains(&crate::forge::LABEL_READY.to_string())
                && !a_labels.contains(&crate::forge::LABEL_PLAN.to_string()),
            "a partially-materialized child stays unlabeled (not discoverable): {a_labels:?}"
        );
        // The proposal PR is still open — finalize (and labeling) has not run.
        assert_eq!(forge.get_pr(10).await.unwrap().state, "open");
    }

    #[test]
    fn parse_children_block_requires_exactly_one_block() {
        assert!(parse_children_block("no block here").is_err());
        let two = format!("{}\n{}", spec_with("[]"), spec_with("[]"),);
        assert!(parse_children_block(&two).is_err());
        let bad_json = spec_with("{not an array}");
        assert!(parse_children_block(&bad_json).is_err());
    }

    #[test]
    fn parse_children_block_reads_the_array_and_keeps_project() {
        let spec = spec_with(
            r#"[{"title":"A","body":"b","kind":"plan","blocked_by":[],"project":"sib"}]"#,
        );
        let kids = parse_children_block(&spec).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].title, "A");
        assert_eq!(kids[0].kind, "plan");
        assert_eq!(kids[0].project.as_deref(), Some("sib"));
    }

    #[test]
    fn parse_ignores_ordinary_json_example_blocks() {
        let spec = "text\n```json\n{\"example\": true}\n```\nmore\n";
        assert!(parse_children_block(spec).is_err()); // no meguri-children block
    }

    #[test]
    fn upsert_block_replaces_not_duplicates() {
        let body = "intro".to_string();
        let once = upsert_block(&body, &format!("{TRACKING_START}\nv1\n{TRACKING_END}"));
        let twice = upsert_block(&once, &format!("{TRACKING_START}\nv2\n{TRACKING_END}"));
        assert!(twice.contains("v2"));
        assert!(!twice.contains("v1"));
        assert_eq!(twice.matches(TRACKING_START).count(), 1);
    }
}
