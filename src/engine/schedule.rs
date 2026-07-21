//! Schedule Kind reconciler (ADR 0012 slice 2, issue #222). The cron-fire
//! poll-tick sweep, folded into the level-triggered vocabulary — **observe →
//! pure `next_step` → act** — and extended to read schedules from the repo's
//! own `meguri.toml` on the default branch (ADR 0026), not just host config.
//!
//! Firing stays **enqueue-only** (ADR 0009): a due schedule creates one
//! issue/task the existing worker/planner discovery consumes — no pane, no run
//! record. The last-fired window lives in sqlite (`schedule_state`), not the
//! forge, so a kill folds catch-up to a single fire (ADR 0012 decision 2). The
//! delivery contract is **at-least-once**: a crash between `enqueue` and
//! `record_schedule_fire` re-fires the same window next tick; the overlap guard
//! does NOT catch that (the new key was never saved), so a duplicate is bounded
//! only by the narrow window and is human-visible (enqueue-only).
//!
//! Repo schedules are a *discovery read* from the default branch (ADR 0015):
//! the resolver fetches `origin/<default>` (bounded, so a hung credential helper
//! can't stall host firing), pins one SHA, reads `meguri.toml` and every
//! `body_file` at it (one snapshot), validates, and merges host-wins. On a fetch
//! failure the repo layer *abstains* (fail-closed) — host schedules still fire.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::Deps;
use crate::config::{ProjectMode, RepoManifest, ScheduleConfig, ScheduleKind};
use crate::cron::{self, Cron};
use crate::forge::{IssueState, LABEL_PLAN, LABEL_READY};
use crate::gitops::{self, DefaultBranchFile};
use crate::notify::Notification;
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

// --- effective-set resolution (host ∪ repo default branch) -----------------

/// Where an effective schedule came from — surfaced in `schedule.fired` and in
/// `meguri schedules` / `doctor` so a repo schedule is never invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleSource {
    Host,
    Repo,
}

impl ScheduleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Repo => "repo",
        }
    }
}

/// One schedule in the effective set. Repo schedules carry the pinned commit
/// they were read at so their `body_file` is read from the same snapshot
/// (issue #222 f5); host schedules share the sweep's pin when the fetch
/// succeeded, else `None` (fall back to the plain default-branch read).
#[derive(Debug, Clone)]
pub struct EffectiveSchedule {
    pub config: ScheduleConfig,
    pub source: ScheduleSource,
    pub pin_sha: Option<String>,
}

/// A level-triggered diagnostic the resolver produces as *data*. The sweep is
/// the only caller that turns these into events, and only edge-triggered (on a
/// transition), because `Store::emit` is an unconditional INSERT and these
/// conditions persist across ticks (issue #222 f6). `doctor` / `meguri
/// schedules` display them without emitting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    /// A repo schedule was dropped because a host schedule owns the name (D5).
    Shadowed { name: String },
    /// The repo schedule set was dropped whole: a TOML/host-only-key parse
    /// error, a wrong-shaped `schedules` field, or a duplicate name (a
    /// collection-level error, D6).
    RepoInvalid { detail: String },
    /// A single repo schedule entry was dropped (a missing required field or a
    /// per-schedule validation failure) while the rest survive (D6 / f1). Kept
    /// distinct from `RepoInvalid` so it is visible in doctor / CLI rather than
    /// silently warned, without dropping the whole set.
    RepoScheduleDropped { detail: String },
    /// The repo-schedule layer abstained this tick: the freshness fetch failed
    /// or timed out (fail-closed, ADR 0026).
    RepoUnavailable { detail: String },
}

impl Diagnostic {
    /// A stable key for edge-triggered dedup. It includes the *detail*, not just
    /// the kind, so a same-kind condition whose cause changes (e.g. a fetch
    /// timeout becoming an auth error) is a new transition and re-emits (issue
    /// #222 f6). The detail is a stable message for a stable cause (no clock /
    /// counter), so the steady state still dedups.
    fn signature(&self) -> String {
        match self {
            Self::Shadowed { name } => format!("shadowed:{name}"),
            Self::RepoInvalid { detail } => format!("repo_invalid:{detail}"),
            Self::RepoScheduleDropped { detail } => format!("repo_schedule_dropped:{detail}"),
            Self::RepoUnavailable { detail } => format!("repo_unavailable:{detail}"),
        }
    }

    /// The event kind emitted when this diagnostic first appears (or its cause
    /// changes). The matching resolution emits `schedule.diagnostic_cleared`.
    fn event_kind(&self) -> &'static str {
        match self {
            Self::Shadowed { .. } => "schedule.shadowed",
            // An invalid whole-set and a dropped entry are both invalid repo
            // config; the payload detail distinguishes them.
            Self::RepoInvalid { .. } | Self::RepoScheduleDropped { .. } => "repo_config.invalid",
            Self::RepoUnavailable { .. } => "schedule.repo_unavailable",
        }
    }
}

/// The resolver's output: the effective schedules to drive, plus diagnostics as
/// data (never emitted here).
#[derive(Debug, Clone, Default)]
pub struct ResolvedSchedules {
    pub schedules: Vec<EffectiveSchedule>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Build the effective schedule set = host `∪` repo(default branch), host-wins
/// (ADR 0026). Shared by [`sweep`], `doctor`, and `meguri schedules` so display
/// and firing see the same set. Pure w.r.t. events — it returns diagnostics as
/// data. Never fails: any repo-side problem degrades to "host only" plus a
/// diagnostic (ADR 0011 "don't kill the process").
pub async fn resolve_effective_schedules(
    repo_path: &Path,
    default_branch: &str,
    mode: ProjectMode,
    host_schedules: &[ScheduleConfig],
) -> ResolvedSchedules {
    let mut diagnostics = Vec::new();

    // The repo layer is gated on a successful, bounded fetch (f1/f4) and a
    // resolvable tip; either failing means abstain (fail-closed, f3). Host
    // schedules never depend on this.
    let pin_sha = match gitops::fetch_default_branch(repo_path, default_branch).await {
        Ok(()) => match gitops::resolve_default_branch_sha(repo_path, default_branch).await {
            Ok(sha) => Some(sha),
            Err(e) => {
                diagnostics.push(Diagnostic::RepoUnavailable {
                    detail: format!("{e:#}"),
                });
                None
            }
        },
        Err(e) => {
            diagnostics.push(Diagnostic::RepoUnavailable {
                detail: format!("{e:#}"),
            });
            None
        }
    };

    // Host schedules always fire; they carry the sweep pin when we have one so
    // their body_file is read from the same snapshot.
    let mut effective: Vec<EffectiveSchedule> = host_schedules
        .iter()
        .map(|c| EffectiveSchedule {
            config: c.clone(),
            source: ScheduleSource::Host,
            pin_sha: pin_sha.clone(),
        })
        .collect();

    if let Some(sha) = &pin_sha {
        let repo =
            resolve_repo_schedules(repo_path, sha, mode, host_schedules, &mut diagnostics).await;
        effective.extend(repo);
    }

    ResolvedSchedules {
        schedules: effective,
        diagnostics,
    }
}

/// Read + validate + host-wins-merge the repo schedules at the pinned SHA.
async fn resolve_repo_schedules(
    repo_path: &Path,
    pin_sha: &str,
    mode: ProjectMode,
    host_schedules: &[ScheduleConfig],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<EffectiveSchedule> {
    let raw = match gitops::read_file_at_ref(repo_path, pin_sha, "meguri.toml").await {
        Ok(DefaultBranchFile::Content(text)) => text,
        // No meguri.toml on the default branch is the valid opt-out — silent.
        Ok(DefaultBranchFile::Absent) => return Vec::new(),
        Ok(DefaultBranchFile::NotRegularFile) => {
            diagnostics.push(Diagnostic::RepoInvalid {
                detail: "meguri.toml on the default branch is not a regular file".to_string(),
            });
            return Vec::new();
        }
        Err(e) => {
            diagnostics.push(Diagnostic::RepoInvalid {
                detail: format!("reading meguri.toml: {e:#}"),
            });
            return Vec::new();
        }
    };

    // Envelope parse: a host-only key or bad TOML is a collection error → drop
    // the whole repo set (D6). A malformed *entry* does not fail this parse.
    let manifest = match RepoManifest::parse_str(&raw) {
        Ok(m) => m,
        Err(e) => {
            diagnostics.push(Diagnostic::RepoInvalid {
                detail: format!("{e:#}"),
            });
            return Vec::new();
        }
    };

    // Interpret the `schedules` field. A wrong *shape* (not an array of tables)
    // is a collection error → drop the whole set (D6 / f2); a bad *entry* drops
    // just itself but is surfaced as a diagnostic so doctor / CLI see it, not a
    // silent warn (D6 / f1).
    let (typed, entry_errs) = match manifest.typed_schedules() {
        Ok(v) => v,
        Err(e) => {
            diagnostics.push(Diagnostic::RepoInvalid {
                detail: format!("{e:#}"),
            });
            return Vec::new();
        }
    };
    for e in &entry_errs {
        diagnostics.push(Diagnostic::RepoScheduleDropped {
            detail: format!("{e:#}"),
        });
    }

    // Collection-level duplicate names → drop the whole set (D6).
    if let Err(e) = crate::config::validate_schedule_set_names(&typed) {
        diagnostics.push(Diagnostic::RepoInvalid {
            detail: format!("{e:#}"),
        });
        return Vec::new();
    }

    // Per-schedule validation → drop only the offending entries (D6), each
    // surfaced as a diagnostic (f1).
    let mut kept = Vec::new();
    for s in typed {
        match crate::config::validate_schedule(mode, &s) {
            Ok(()) => kept.push(s),
            Err(e) => diagnostics.push(Diagnostic::RepoScheduleDropped {
                detail: format!("schedule {:?}: {e:#}", s.name),
            }),
        }
    }
    let typed = kept;

    // Host-wins merge: a repo schedule whose name a host schedule already owns
    // is shadowed (D5).
    let host_names: HashSet<&str> = host_schedules.iter().map(|s| s.name.as_str()).collect();
    let mut out = Vec::new();
    for s in typed {
        if host_names.contains(s.name.as_str()) {
            diagnostics.push(Diagnostic::Shadowed {
                name: s.name.clone(),
            });
            continue;
        }
        out.push(EffectiveSchedule {
            config: s,
            source: ScheduleSource::Repo,
            pin_sha: Some(pin_sha.to_string()),
        });
    }
    out
}

// --- pure decision (observe → next_step → act) -----------------------------

/// The pure inputs [`next_step`] decides on: no wall-clock, no I/O. Deliberately
/// total so a property test can enumerate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// A `schedule_state` row already exists (else the first observation seeds).
    pub seeded: bool,
    /// A cron occurrence falls in the window `(max(last_fired, first_seen), now]`.
    pub due: bool,
    /// The definition allows overlapping the last-created item.
    pub allow_overlap: bool,
    /// The last-created item is still open (only consulted when `due &&
    /// !allow_overlap`; `false` otherwise — `next_step` gates it behind `due`).
    pub last_item_open: bool,
}

/// The decision `next_step` returns for one schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// First observation: seed the window bottom, do not fire (no backfill).
    Seed,
    /// Due and clear: enqueue one item and record the fire.
    Fire,
    /// Due but the last item is still open: consume the window, keep the key.
    SkipOverlap,
    /// Not due — the owning arm intentionally stays idle this tick.
    Wait,
}

/// The pure decision (ADR 0012 §3). Same [`Snapshot`] ⇒ same [`Step`].
pub fn next_step(s: &Snapshot) -> Step {
    if !s.seeded {
        return Step::Seed;
    }
    if !s.due {
        return Step::Wait;
    }
    if !s.allow_overlap && s.last_item_open {
        return Step::SkipOverlap;
    }
    Step::Fire
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

// --- sweep -----------------------------------------------------------------

/// Per-project memory of which diagnostics were emitted last tick, so the sweep
/// emits each only on its transition (issue #222 f6). Lives in the watch loop
/// (not sqlite); a restart re-emits each once, which is fine.
pub type DiagMemory = HashMap<String, HashSet<String>>;

/// Poll-tick sweep for one project: resolve the effective set, then drive each
/// schedule through `next_step` → act. A per-schedule failure warns and is
/// retried next tick; it never aborts the sweep.
pub async fn sweep(deps: &Deps, now: u64, diag_memory: &mut DiagMemory) -> Result<()> {
    let resolved = resolve_effective_schedules(
        &deps.repo_path(),
        &deps.project.default_branch,
        deps.project.mode,
        &deps.project.schedules,
    )
    .await;

    emit_diagnostics_edge_triggered(deps, &resolved.diagnostics, diag_memory).await;

    for sched in &resolved.schedules {
        if let Err(e) = fire_one(deps, sched, now).await {
            let detail = format!("{e:#}");
            tracing::warn!(
                "schedule {:?} failed for {}: {detail}",
                sched.config.name,
                deps.project.id
            );
            // Surface the failure as an event (issue #205) and page a human if
            // `schedule.failed` is subscribed. Best-effort: a failed
            // notification must not abort the sweep.
            let _ = deps.store.emit(
                None,
                "schedule.failed",
                json!({ "project": deps.project.id, "schedule": sched.config.name, "error": detail }),
            );
            deps.notifier
                .notify(&Notification::schedule_failed(
                    &deps.project.id,
                    &sched.config.name,
                    &detail,
                ))
                .await;
        }
    }
    Ok(())
}

/// Emit resolver diagnostics only on their **transitions** — appearance, cause
/// change, and resolution — never every tick (issue #222 f6). `doctor` /
/// `meguri schedules` never call this; they display the diagnostics instead.
async fn emit_diagnostics_edge_triggered(
    deps: &Deps,
    diagnostics: &[Diagnostic],
    diag_memory: &mut DiagMemory,
) {
    // Signature → diagnostic for this tick. The signature carries the detail, so
    // a same-kind cause change is a new signature (a fresh appearance).
    let current: HashMap<String, &Diagnostic> =
        diagnostics.iter().map(|d| (d.signature(), d)).collect();
    let previous = diag_memory.entry(deps.project.id.clone()).or_default();

    // Appearance / change: a signature not present last tick.
    for (sig, d) in &current {
        if previous.contains(sig) {
            continue; // steady state — already emitted on its transition
        }
        let data = match d {
            Diagnostic::Shadowed { name } => {
                tracing::warn!(
                    "schedule {name:?} in {}'s repo meguri.toml is shadowed by a host schedule",
                    deps.project.id
                );
                json!({ "project": deps.project.id, "name": name })
            }
            Diagnostic::RepoInvalid { detail } => {
                tracing::warn!(
                    "repo meguri.toml schedules for {} are invalid: {detail}",
                    deps.project.id
                );
                json!({ "project": deps.project.id, "error": detail })
            }
            Diagnostic::RepoScheduleDropped { detail } => {
                tracing::warn!("repo schedule dropped for {}: {detail}", deps.project.id);
                json!({ "project": deps.project.id, "error": detail, "scope": "entry" })
            }
            Diagnostic::RepoUnavailable { detail } => {
                tracing::warn!(
                    "repo schedules for {} unavailable this tick (fetch): {detail}",
                    deps.project.id
                );
                json!({ "project": deps.project.id, "error": detail })
            }
        };
        let _ = deps.store.emit(None, d.event_kind(), data);
    }

    // Resolution: a signature present last tick but gone now (fetch recovered,
    // config fixed, shadow removed). Emit one `schedule.diagnostic_cleared`.
    for sig in previous.iter() {
        if !current.contains_key(sig) {
            let _ = deps.store.emit(
                None,
                "schedule.diagnostic_cleared",
                json!({ "project": deps.project.id, "diagnostic": sig }),
            );
            tracing::info!("schedule diagnostic cleared for {}: {sig}", deps.project.id);
        }
    }

    *previous = current.keys().cloned().collect();
}

async fn fire_one(deps: &Deps, sched: &EffectiveSchedule, now: u64) -> Result<()> {
    let cfg = &sched.config;
    let cron =
        Cron::parse(&cfg.cron).map_err(|e| anyhow::anyhow!("invalid cron {:?}: {e}", cfg.cron))?;

    let state = deps.store.get_schedule_state(&deps.project.id, &cfg.name)?;
    let Some(state) = state else {
        // First observation: seed the window bottom and do not fire (no backfill).
        deps.store
            .seed_schedule(&deps.project.id, &cfg.name, &format_epoch(now))?;
        return Ok(());
    };

    let first_seen = parse_ts(&state.first_seen_at).unwrap_or(now);
    let last_fired = state.last_fired_at.as_deref().and_then(parse_ts);
    let due = is_due(&cron, first_seen, last_fired, now);

    // Resolve openness only when it can matter (due && !allow_overlap), so a
    // healthy tick never queries the forge — mirrors the merge-tail's lazy
    // resolution. An unreadable forge state propagates as a per-schedule error
    // (retried next tick), never a fire, so we can't double-fire on a hiccup.
    let last_item_open = if due && !cfg.allow_overlap {
        match state.last_key {
            Some(key) => last_item_open(deps, key).await?,
            None => false,
        }
    } else {
        false
    };

    let snap = Snapshot {
        seeded: true,
        due,
        allow_overlap: cfg.allow_overlap,
        last_item_open,
    };

    match next_step(&snap) {
        Step::Seed => unreachable!("seeded is true here"),
        Step::Wait => Ok(()),
        Step::SkipOverlap => skip_overlap(deps, cfg, now, state.last_key).await,
        Step::Fire => fire(deps, sched, now).await,
    }
}

/// Due but the last item is still open: consume the window (advance it, keep the
/// key) so closing the item later does not backfill the skipped fire.
async fn skip_overlap(
    deps: &Deps,
    cfg: &ScheduleConfig,
    now: u64,
    last_key: Option<i64>,
) -> Result<()> {
    deps.store
        .record_schedule_fire(&deps.project.id, &cfg.name, &format_epoch(now), None)?;
    deps.store.emit(
        None,
        "schedule.skipped",
        json!({ "project": deps.project.id, "schedule": cfg.name, "open_key": last_key }),
    )?;
    deps.notifier
        .notify(&Notification::schedule_skipped(
            &deps.project.id,
            &cfg.name,
            last_key.unwrap_or_default(),
        ))
        .await;
    Ok(())
}

/// Fire: enqueue one item and record the new key. The body is read from the
/// resolver's pinned SHA so definition and body come from one snapshot (f5).
async fn fire(deps: &Deps, sched: &EffectiveSchedule, now: u64) -> Result<()> {
    let cfg = &sched.config;
    let title = render_title(&cfg.title, now);
    let body = render_body(deps, sched).await?;
    let key = enqueue(deps, cfg, &title, &body).await?;
    deps.store
        .record_schedule_fire(&deps.project.id, &cfg.name, &format_epoch(now), Some(key))?;
    deps.store.emit(
        None,
        "schedule.fired",
        json!({ "project": deps.project.id, "schedule": cfg.name,
                "kind": cfg.kind.as_str(), "source": sched.source.as_str(), "key": key }),
    )?;
    tracing::info!(
        project = deps.project.id,
        schedule = cfg.name,
        source = sched.source.as_str(),
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

/// Create the issue (github) or local task (local) and return its key.
async fn enqueue(deps: &Deps, cfg: &ScheduleConfig, title: &str, body: &str) -> Result<i64> {
    match &deps.forge {
        Some(forge) => {
            let label = match cfg.kind {
                ScheduleKind::Ready => LABEL_READY,
                ScheduleKind::Plan => LABEL_PLAN,
            };
            let number = forge.create_issue(title, body, &[label]).await?;
            deps.notify_created_issue(number, title, &[label]).await;
            Ok(number)
        }
        None => {
            // local mode: `kind = "plan"` is rejected at validation, so this is
            // always worker work. `origin` carries the schedule provenance.
            let origin = format!("schedule:{}", cfg.name);
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
/// provenance marker. `body_file` is read at the resolver's pinned SHA when the
/// schedule carries one (issue #222 f5), else from the default branch (host
/// schedules on a fetch-failed tick) — never the working tree (ADR 0015).
async fn render_body(deps: &Deps, sched: &EffectiveSchedule) -> Result<String> {
    let cfg = &sched.config;
    let base = match (&cfg.body, &cfg.body_file) {
        (Some(inline), _) => inline.clone(),
        (None, Some(rel)) => {
            let repo = deps.repo_path();
            let read = match &sched.pin_sha {
                Some(sha) => gitops::read_file_at_ref(&repo, sha, rel).await,
                None => {
                    gitops::read_file_at_default_branch(&repo, &deps.project.default_branch, rel)
                        .await
                }
            }
            .with_context(|| format!("schedule {:?}: reading body_file {rel}", cfg.name))?;
            match read {
                DefaultBranchFile::Content(text) => text,
                DefaultBranchFile::Absent => bail!(
                    "schedule {:?}: body_file {rel} is not on the default branch",
                    cfg.name
                ),
                DefaultBranchFile::NotRegularFile => bail!(
                    "schedule {:?}: body_file {rel} is not a regular file on the default branch",
                    cfg.name
                ),
            }
        }
        (None, None) => String::new(), // unreachable after validation
    };
    Ok(format!("{base}\n\n{}", schedule_marker(&cfg.name)))
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
        assert!(is_due(&cron, first_seen, None, ts("2026-07-13T09:30:00Z")));
        assert!(!is_due(&cron, first_seen, None, ts("2026-07-13T08:30:00Z")));
    }

    #[test]
    fn not_due_again_after_firing_same_day() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        let first_seen = ts("2026-07-13T00:00:00Z");
        let fired = ts("2026-07-13T09:30:00Z");
        assert!(!is_due(
            &cron,
            first_seen,
            Some(fired),
            ts("2026-07-13T18:00:00Z")
        ));
        assert!(is_due(
            &cron,
            first_seen,
            Some(fired),
            ts("2026-07-14T09:05:00Z")
        ));
    }

    #[test]
    fn downtime_folds_to_one() {
        let cron = Cron::parse("0 * * * *").unwrap();
        let first_seen = ts("2026-07-13T00:00:00Z");
        let fired = ts("2026-07-13T00:30:00Z");
        assert!(is_due(
            &cron,
            first_seen,
            Some(fired),
            ts("2026-07-13T06:10:00Z")
        ));
    }

    #[test]
    fn no_backfill_right_after_first_seen() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        let first_seen = ts("2026-07-13T09:30:00Z");
        assert!(!is_due(&cron, first_seen, None, ts("2026-07-13T09:31:00Z")));
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

    #[test]
    fn next_step_ownership_is_total_and_single() {
        // Enumerate the whole Snapshot state space (ADR 0012 §3): every combo
        // maps to exactly one Step, with no gap and no double ownership.
        for &seeded in &[false, true] {
            for &due in &[false, true] {
                for &allow_overlap in &[false, true] {
                    for &last_item_open in &[false, true] {
                        let s = Snapshot {
                            seeded,
                            due,
                            allow_overlap,
                            last_item_open,
                        };
                        let step = next_step(&s);
                        let expected = if !seeded {
                            Step::Seed
                        } else if !due {
                            Step::Wait
                        } else if !allow_overlap && last_item_open {
                            Step::SkipOverlap
                        } else {
                            Step::Fire
                        };
                        assert_eq!(step, expected, "{s:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn first_observation_seeds_never_fires() {
        // The no-backfill invariant at the pure-decision level: an unseeded
        // schedule always seeds, regardless of due/overlap.
        for &due in &[false, true] {
            let s = Snapshot {
                seeded: false,
                due,
                allow_overlap: false,
                last_item_open: false,
            };
            assert_eq!(next_step(&s), Step::Seed);
        }
    }

    #[test]
    fn overlap_guard_only_when_due_and_not_allowed() {
        let base = Snapshot {
            seeded: true,
            due: true,
            allow_overlap: false,
            last_item_open: true,
        };
        assert_eq!(next_step(&base), Step::SkipOverlap);
        // allow_overlap fires even with an open item.
        assert_eq!(
            next_step(&Snapshot {
                allow_overlap: true,
                ..base
            }),
            Step::Fire
        );
        // Not due → wait, open item irrelevant.
        assert_eq!(next_step(&Snapshot { due: false, ..base }), Step::Wait);
    }

    #[test]
    fn diagnostic_signature_tracks_the_cause_not_just_the_kind() {
        // issue #222 f6: a same-kind diagnostic whose cause changes is a new
        // transition (re-emits); an identical cause dedups.
        let timeout = Diagnostic::RepoUnavailable {
            detail: "fetch timed out".into(),
        };
        let auth = Diagnostic::RepoUnavailable {
            detail: "auth failed".into(),
        };
        assert_ne!(timeout.signature(), auth.signature());
        assert_eq!(
            timeout.signature(),
            Diagnostic::RepoUnavailable {
                detail: "fetch timed out".into()
            }
            .signature()
        );
        // The clear event kind is stable per diagnostic kind.
        assert_eq!(timeout.event_kind(), "schedule.repo_unavailable");
    }
}
