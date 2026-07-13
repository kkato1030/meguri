//! The plan→impl handoff sweep for separate delivery (ADR 0008 §6).
//!
//! When `plan_delivery = separate`, the spec/ADR PR is its own PR that a human
//! (or auto-merge) merges on its own; because that PR references its issue with
//! a non-closing `Refs #N`, merging it leaves the issue open in the `speccing`
//! phase. This sweep is the receiver: it spots a `speccing` issue whose spec PR
//! has merged and flips it `speccing → ready`, so the normal worker picks up
//! the implementation in a fresh PR.
//!
//! Like the reaper / auto-merger it rides the watch poll — a light API sweep
//! with no run record or pane. It is idempotent: the flip removes `speccing`,
//! so a handed-off issue is never processed again. `combined` delivery does not
//! use this path (the spec worker takes over the branch instead), so the sweep
//! is inert there.

use anyhow::Result;
use serde_json::json;

use super::Deps;
use super::planner;
use crate::config::PlanDelivery;
use crate::forge;

/// Flip every `speccing` issue whose spec PR merged to `ready` (separate mode).
pub async fn sweep(deps: &Deps) -> Result<()> {
    if deps.forge.is_none() {
        return Ok(()); // local mode has no planner / PRs
    }
    if deps.project.plan_delivery != PlanDelivery::Separate {
        return Ok(()); // combined delivery hands off via the spec worker
    }
    for issue in deps
        .forge()
        .list_issues_with_label(forge::LABEL_SPECCING)
        .await?
    {
        if let Err(e) = process_issue(deps, issue.number, &issue.labels).await {
            tracing::warn!("handoff sweep failed for issue #{}: {e:#}", issue.number);
        }
    }
    Ok(())
}

/// One issue through the handoff. A held / claimed / escalated issue is left
/// alone; an issue whose spec PR is not merged yet simply waits.
async fn process_issue(deps: &Deps, issue: i64, labels: &[String]) -> Result<()> {
    let has = |l: &str| labels.iter().any(|x| x == l);
    if has(forge::LABEL_WORKING) || has(forge::LABEL_HOLD) || has(forge::LABEL_NEEDS_HUMAN) {
        return Ok(());
    }
    // The spec PR branch the planner recorded for this issue.
    let Some(branch) = deps
        .store
        .branch_for_issue(&deps.project.id, planner::KIND, issue)?
    else {
        return Ok(());
    };
    let Some(pr) = deps.forge().pr_for_branch(&branch).await? else {
        return Ok(());
    };
    // Only a *merged* spec PR advances the issue. An open one is still under
    // review / awaiting merge; a closed-unmerged one was abandoned (a human
    // re-triages — meguri must not silently implement against a rejected spec).
    if pr.state != "merged" {
        return Ok(());
    }

    // Handoff: speccing → ready. The `ready` add is load-bearing (the worker
    // keys off it), so a failure fails the sweep step; the `speccing` removal
    // is best-effort like every other phase swap.
    deps.forge().add_label(issue, forge::LABEL_READY).await?;
    deps.forge()
        .remove_label(issue, forge::LABEL_SPECCING)
        .await
        .ok();
    deps.forge()
        .comment(
            issue,
            &format!(
                "🔁 **meguri** — spec/ADR PR #{} がマージされたので、この issue を \
                 `{}` に張り替えました。実装を別 PR で進めます。",
                pr.number,
                forge::LABEL_READY
            ),
        )
        .await
        .ok();
    deps.store.emit(
        None,
        "handoff.speccing_to_ready",
        json!({ "issue": issue, "spec_pr": pr.number }),
    )?;
    tracing::info!("handoff: issue #{issue} speccing → ready (spec PR #{})", pr.number);
    Ok(())
}
