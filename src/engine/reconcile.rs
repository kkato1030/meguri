//! The reconcile sweep (issue #142, half B): a light poll-riding pass — like
//! the reaper and auto-merger — that notices when a *once-shipped* issue's body
//! was edited and leaves a re-attention signal.
//!
//! Why a sweep and not the label discovery: after a worker success the issue's
//! phase label swaps `ready` → `implementing` (ADR 0005), so it drops out of
//! `list_issues_with_label(ready)`. A later body edit is then invisible to the
//! label-filtered worker discovery (half A only sees issues that still carry a
//! trigger label). This sweep looks at the `implementing` lane instead.
//!
//! What it does NOT do: it never launches an agent. The body edit is a signal,
//! not a trigger — the execution gate stays the collaborator-applied label
//! (ADR 0008). The sweep emits `issue.body_changed` and, unless disabled, posts
//! one comment nudging a human to re-label `meguri:ready` if a re-run is wanted.
//! Both are deduped per new-body digest (shared `issue_reconcile` table with
//! half A), so an edit waiting to be reprocessed does not re-fire every tick.

use anyhow::Result;

use super::{Deps, worker};
use crate::forge;
use crate::tasks::body_digest;

/// The re-attention comment left on an issue whose body changed after a
/// succeeded run. Deliberately points at the collaborator-gated label, not at
/// any automatic action.
const SIGNAL_COMMENT: &str = "🔁 **meguri**: この issue の本文が更新されました。\
     再実装が必要なら、フェーズラベルを `meguri:ready` に付け直してください \
     (本文の編集そのものでは自動起動しません — 起動ゲートはラベルのままです)。";

/// One reconcile pass over a project's `implementing` issues. Errors are
/// returned to the scheduler, which logs and rides on to the next tick (like
/// the other sweeps).
pub async fn sweep(deps: &Deps) -> Result<()> {
    // Kill switch off, or local mode (no forge / no phase labels): inert.
    if !deps.config.reconcile.body_edits {
        return Ok(());
    }
    let Some(forge) = deps.forge.as_ref() else {
        return Ok(());
    };

    for issue in forge
        .list_issues_with_label(forge::LABEL_IMPLEMENTING)
        .await?
    {
        // Only issues meguri actually shipped through the worker lane: a
        // succeeded worker run is what recorded a body digest to compare
        // against. (spec-worker-shipped issues keep their spec-ready PR, which
        // half A watches — no need to double-cover them here.)
        if !deps
            .store
            .issue_has_succeeded_run(&deps.project.id, worker::KIND, issue.number)?
        {
            continue;
        }
        let digest = body_digest(&issue.body);
        // Body unchanged since the last worker success → nothing to signal.
        if deps.store.issue_processed_current_body(
            &deps.project.id,
            worker::KIND,
            issue.number,
            &digest,
        )? {
            continue;
        }
        // Changed body, but this exact new body was already signaled (by an
        // earlier tick or half A) → stay quiet.
        if !deps
            .store
            .reconcile_needs_signal(&deps.project.id, issue.number, &digest)?
        {
            continue;
        }

        let run_id =
            deps.store
                .latest_succeeded_run_id(&deps.project.id, worker::KIND, issue.number)?;
        deps.store.emit(
            run_id.as_deref(),
            "issue.body_changed",
            serde_json::json!({
                "issue": issue.number,
                "loop": worker::KIND,
                "digest": digest,
                "source": "reconcile-sweep",
            }),
        )?;
        // The comment is best-effort: a forge hiccup must not stop the sweep,
        // and the durable signal is the event above. Skipped when disabled.
        if deps.config.reconcile.signal_comment {
            let _ = forge.comment(issue.number, SIGNAL_COMMENT).await;
        }
        deps.store
            .mark_reconcile_signaled(&deps.project.id, issue.number, &digest)?;
    }
    Ok(())
}
