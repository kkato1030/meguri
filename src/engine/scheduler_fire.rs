//! Time-driven enqueue (issue #146): the `[[projects.schedules]]` sweep. Like
//! `reaper::sweep` / `auto_merger::sweep`, this is an out-of-band poll-tick
//! sweep, not a `Loop` — firing a schedule only *creates one issue/task*
//! (github: `forge.create_issue`; local: `store.create_task`), which the
//! existing worker/planner discovery then consumes (ADR 0009). No pane, no run
//! record.
//!
//! State lives in sqlite (`schedule_state`), not config, so a hot-reload edit
//! to a definition never loses the last-fired time. The firing window is
//! `(max(last_fired, first_seen), now]`; if any cron occurrence falls in it we
//! fire *once* (catch-up is folded, the cron-daemon rule). A schedule seen for
//! the first time is seeded without firing, so adding one never backfills the
//! past. The default overlap guard skips (but still consumes the window) while
//! the schedule's last-created item is still open.

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::Deps;
use crate::config::{ScheduleConfig, ScheduleKind};
use crate::cron::{self, Cron};
use crate::forge::{IssueState, LABEL_PLAN, LABEL_READY};
use crate::store::{format_epoch, parse_ts};

/// Hidden provenance marker embedded in a fired issue/task body — the same
/// idiom as the cleaner's head-sha marker (`src/engine/cleaner.rs`). Lets a
/// human (or a future tool) see which schedule produced an item.
pub fn schedule_marker(name: &str) -> String {
    format!("<!-- meguri:schedule name={name} -->")
}

/// Epoch seconds now (the injected-clock seam: the scheduler passes this into
/// [`sweep`], tests pass a fixed value).
pub fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure due decision: does a cron occurrence fall in the window
/// `(max(last_fired, first_seen), now]`? Folds catch-up — the answer is the
/// same whether one or a hundred occurrences fall in the window.
fn is_due(cron: &Cron, first_seen: u64, last_fired: Option<u64>, now: u64) -> bool {
    let lo = last_fired.unwrap_or(first_seen).max(first_seen);
    match cron.next_after(lo) {
        Some(next) => next <= now,
        None => false,
    }
}

/// Poll-tick sweep for one project: fire every due schedule. A per-schedule
/// failure warns and is retried next tick; it never aborts the sweep.
pub async fn sweep(deps: &Deps, now: u64) -> Result<()> {
    for sched in &deps.project.schedules {
        if let Err(e) = fire_one(deps, sched, now).await {
            tracing::warn!(
                "schedule {:?} failed for {}: {e:#}",
                sched.name,
                deps.project.id
            );
        }
    }
    Ok(())
}

async fn fire_one(deps: &Deps, sched: &ScheduleConfig, now: u64) -> Result<()> {
    // Parsed once per fire. The definition is validated at config load, so a
    // parse error here is a genuine surprise (warned by the caller).
    let cron = Cron::parse(&sched.cron)
        .map_err(|e| anyhow::anyhow!("invalid cron {:?}: {e}", sched.cron))?;

    let state = deps
        .store
        .get_schedule_state(&deps.project.id, &sched.name)?;
    let Some(state) = state else {
        // First observation: seed the window bottom and do not fire, so a new
        // schedule never backfills.
        deps.store
            .seed_schedule(&deps.project.id, &sched.name, &format_epoch(now))?;
        return Ok(());
    };

    let first_seen = parse_ts(&state.first_seen_at).unwrap_or(now);
    let last_fired = state.last_fired_at.as_deref().and_then(parse_ts);
    if !is_due(&cron, first_seen, last_fired, now) {
        return Ok(());
    }

    // Due. Default overlap guard: while the last-created item is still open,
    // skip — but consume this occurrence (advance the window, keep the key) so
    // closing the item later does not backfill the skipped fire.
    if !sched.allow_overlap
        && let Some(key) = state.last_key
        && last_item_open(deps, key).await?
    {
        deps.store
            .record_schedule_fire(&deps.project.id, &sched.name, &format_epoch(now), None)?;
        deps.store.emit(
            None,
            "schedule.skipped",
            json!({ "project": deps.project.id, "schedule": sched.name, "open_key": key }),
        )?;
        return Ok(());
    }

    // Fire: enqueue one item and record the new key.
    let title = render_title(&sched.title, now);
    let body = render_body(deps, sched).await?;
    let key = enqueue(deps, sched, &title, &body).await?;
    deps.store.record_schedule_fire(
        &deps.project.id,
        &sched.name,
        &format_epoch(now),
        Some(key),
    )?;
    deps.store.emit(
        None,
        "schedule.fired",
        json!({ "project": deps.project.id, "schedule": sched.name,
                "kind": sched.kind.as_str(), "key": key }),
    )?;
    tracing::info!(
        project = deps.project.id,
        schedule = sched.name,
        key,
        "schedule fired"
    );
    Ok(())
}

/// Is the schedule's last-created item still open? github: issue/PR state;
/// local: the task is not in a terminal status.
async fn last_item_open(deps: &Deps, key: i64) -> Result<bool> {
    match &deps.forge {
        Some(forge) => Ok(matches!(forge.issue_state(key).await?, IssueState::Open)),
        None => match deps.store.get_task(key)? {
            Some(t) => Ok(!matches!(t.status.as_str(), "done" | "cancelled")),
            None => Ok(false),
        },
    }
}

/// Create the issue (github) or local task (local) and return its key (issue
/// number / task id).
async fn enqueue(deps: &Deps, sched: &ScheduleConfig, title: &str, body: &str) -> Result<i64> {
    match &deps.forge {
        Some(forge) => {
            let label = match sched.kind {
                ScheduleKind::Ready => LABEL_READY,
                ScheduleKind::Plan => LABEL_PLAN,
            };
            forge.create_issue(title, body, &[label]).await
        }
        None => {
            // local mode: `kind = "plan"` is rejected at config load, so this
            // is always worker work. `origin` carries the schedule provenance.
            let origin = format!("schedule:{}", sched.name);
            let task = deps
                .store
                .create_task(&deps.project.id, "work", title, body, &origin)?;
            Ok(task.id)
        }
    }
}

/// Substitute the minimal title template (`{{date}}` → the fire date, UTC).
fn render_title(template: &str, now: u64) -> String {
    template.replace("{{date}}", &cron::date_utc(now))
}

/// Resolve the body from `body` (inline) or `body_file`, then append the hidden
/// provenance marker. Exactly one source is present (config validation
/// guarantees it). `body_file` is read from the default branch, not the working
/// tree (ADR 0015), so a working-tree-only edit never reaches an issue body.
async fn render_body(deps: &Deps, sched: &ScheduleConfig) -> Result<String> {
    let base = match (&sched.body, &sched.body_file) {
        (Some(inline), _) => inline.clone(),
        (None, Some(rel)) => {
            let read = crate::gitops::read_file_at_default_branch(
                &deps.project.repo_path,
                &deps.project.default_branch,
                rel,
            )
            .await
            .with_context(|| format!("schedule {:?}: reading body_file {rel}", sched.name))?;
            match read {
                crate::gitops::DefaultBranchFile::Content(text) => text,
                crate::gitops::DefaultBranchFile::Absent => bail!(
                    "schedule {:?}: body_file {rel} is not on the default branch",
                    sched.name
                ),
                crate::gitops::DefaultBranchFile::NotRegularFile => bail!(
                    "schedule {:?}: body_file {rel} is not a regular file on the default branch",
                    sched.name
                ),
            }
        }
        (None, None) => String::new(), // unreachable after validation
    };
    Ok(format!("{base}\n\n{}", schedule_marker(&sched.name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::parse_ts;

    fn ts(s: &str) -> u64 {
        parse_ts(s).unwrap()
    }

    #[test]
    fn due_when_occurrence_in_window() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        let first_seen = ts("2026-07-13T00:00:00Z");
        // now is 09:30, no prior fire: the 09:00 occurrence is in the window.
        assert!(is_due(&cron, first_seen, None, ts("2026-07-13T09:30:00Z")));
        // now is 08:30: 09:00 has not happened yet.
        assert!(!is_due(&cron, first_seen, None, ts("2026-07-13T08:30:00Z")));
    }

    #[test]
    fn not_due_again_after_firing_same_day() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        let first_seen = ts("2026-07-13T00:00:00Z");
        let fired = ts("2026-07-13T09:30:00Z"); // window consumed up to 09:30
        // Later the same day there is no new occurrence.
        assert!(!is_due(
            &cron,
            first_seen,
            Some(fired),
            ts("2026-07-13T18:00:00Z")
        ));
        // Next day's 09:00 is due again.
        assert!(is_due(
            &cron,
            first_seen,
            Some(fired),
            ts("2026-07-14T09:05:00Z")
        ));
    }

    #[test]
    fn downtime_folds_to_one() {
        // Hourly schedule, down for several hours: still just "due once".
        let cron = Cron::parse("0 * * * *").unwrap();
        let first_seen = ts("2026-07-13T00:00:00Z");
        let fired = ts("2026-07-13T00:30:00Z");
        // Six occurrences (01:00..06:00) elapsed, but is_due is a single bool —
        // the sweep fires once and advances the window to `now`.
        assert!(is_due(
            &cron,
            first_seen,
            Some(fired),
            ts("2026-07-13T06:10:00Z")
        ));
    }

    #[test]
    fn no_backfill_right_after_first_seen() {
        // first_seen is 09:30; today's 09:00 already passed — must not fire.
        let cron = Cron::parse("0 9 * * *").unwrap();
        let first_seen = ts("2026-07-13T09:30:00Z");
        assert!(!is_due(&cron, first_seen, None, ts("2026-07-13T09:31:00Z")));
        // Tomorrow's 09:00 fires.
        assert!(is_due(&cron, first_seen, None, ts("2026-07-14T09:05:00Z")));
    }

    #[test]
    fn marker_roundtrip() {
        assert_eq!(
            schedule_marker("daily-tidy"),
            "<!-- meguri:schedule name=daily-tidy -->"
        );
    }

    #[test]
    fn title_template_substitutes_date() {
        assert_eq!(
            render_title("Daily tidy {{date}}", ts("2026-07-13T09:00:00Z")),
            "Daily tidy 2026-07-13"
        );
        assert_eq!(render_title("No vars", 0), "No vars");
    }
}
