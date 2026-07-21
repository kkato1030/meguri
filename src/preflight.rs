//! Launch-time pre-flight prime (issue #235).
//!
//! An interactive agent pane (worker / planner / fixer / pr-reviewer — ADR
//! 0012 `Pane` mode) stalls on the CLI's first-run *folder-trust* prompt for a
//! fresh worktree: meguri never reads the screen, so nobody answers it. This
//! module runs the CLI's own headless one-shot in the worktree cwd *just
//! before* the pane spawns, so the CLI records folder trust for that path and
//! the real pane starts clean.
//!
//! Two properties make the prime safe to run automatically (ADR 0027):
//!
//! - **It writes only folder trust, never bypass acceptance.** The prime never
//!   carries yolo (`--dangerously-skip-permissions`); bypass acceptance stays
//!   doctor's one-time, config-dir-level concern (issue #234).
//! - **The prime turn executes no tool.** It runs under a meguri-owned
//!   deny-all `--settings` file plus `--strict-mcp-config` (no MCP), so a
//!   hostile `CLAUDE.md` in the worktree cannot drive Bash/Edit/MCP before the
//!   pane starts. `deny` wins over any inherited `allow`, so a permissive
//!   config-dir cannot re-enable tools.
//!
//! The argv (including the deny file and the model carried over from the
//! pane's profile) is resolved by [`crate::routing::effective_preflight_args`];
//! this module owns *running* it once per (identity, path), serialized and
//! claim-once, plus writing the deny file.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{self, AgentProfile};
use crate::gate::REAP_DEADLINE;
use crate::routing;

/// How long the prime may run before it is killed and the pane launched
/// anyway. Longer than the doctor gate-probe's 8s because the prime is a real
/// (if trivial) model turn — a network round-trip — not just a screen read.
pub const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// The meguri-owned deny-all settings file the prime runs under (ADR 0027 D1
/// [prime 仕様]). `deny` covers every current built-in tool plus `mcp__*`;
/// `defaultMode: "plan"` is defence in depth; the two MCP keys disable project
/// MCP servers (belt-and-suspenders with `--strict-mcp-config`).
const DENY_SETTINGS_JSON: &str = r#"{
  "permissions": {
    "deny": ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "WebFetch", "WebSearch", "Task", "NotebookEdit", "TodoWrite", "mcp__*"],
    "defaultMode": "plan"
  },
  "enableAllProjectMcpServers": false,
  "enabledMcpjsonServers": []
}
"#;

/// The result of a pre-flight attempt, mapped by the caller to an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// The prime ran and exited 0 (`preflight.ran`).
    Ran { duration_ms: u64 },
    /// The prime ran but failed — spawn error / nonzero exit / timeout, or the
    /// deny file could not be written (`preflight.failed`). The pane still
    /// launches (best-effort); the failure is recorded and not retried.
    Failed { reason: String, duration_ms: u64 },
    /// No prime was run because the profile resolved to no prime argv
    /// (`preflight.skipped`): an older/unknown `claude`, a non-`claude`
    /// command, or an explicit `preflight = []` opt-out.
    Skipped { reason: &'static str },
    /// This (identity, path) was already primed once — nothing to do, no event.
    AlreadyDone,
}

/// Absolute path of the deny-all settings file (no I/O).
pub fn deny_settings_path() -> PathBuf {
    config::preflight_dir().join("deny.json")
}

/// Write the deny-all settings file idempotently with `0600` permissions.
fn ensure_deny_settings() -> Result<PathBuf> {
    write_deny_settings_in(&config::preflight_dir())
}

/// Testable core of [`ensure_deny_settings`]: write `deny.json` under `dir`.
fn write_deny_settings_in(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating preflight dir {}", dir.display()))?;
    let path = dir.join("deny.json");
    let needs_write = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != DENY_SETTINGS_JSON,
        Err(_) => true,
    };
    if needs_write {
        std::fs::write(&path, DENY_SETTINGS_JSON)
            .with_context(|| format!("writing deny settings {}", path.display()))?;
    }
    set_owner_only(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

/// Short hex hash of the gate identity + target path — the marker filename and
/// the serialization-lock key. Two spawns share a prime iff they agree on all
/// four (issue #235 f6/f7): same CLI, same config-dir, same prime argv, same
/// primed cwd.
fn identity_hash(command: &str, config_dir: &Path, argv: &[String], target: &Path) -> String {
    let mut h = DefaultHasher::new();
    command.hash(&mut h);
    config_dir.hash(&mut h);
    argv.hash(&mut h);
    target.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Process-wide registry of per-identity async locks. Serializes
/// check→prime→record so concurrent first-spawns of the same worktree prime
/// exactly once (issue #235 f7); late arrivals block, then see the marker and
/// skip. Different identities/paths use different keys and run concurrently.
static PREFLIGHT_LOCKS: LazyLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_for(key: &str) -> Arc<AsyncMutex<()>> {
    let mut map = PREFLIGHT_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(key.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

fn base_name(command: &str) -> &str {
    Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
}

/// Why the prime resolved to no argv (for the `preflight.skipped` event).
fn skip_reason(profile: &AgentProfile, version: Option<(u64, u64, u64)>) -> &'static str {
    if matches!(&profile.preflight, Some(v) if v.is_empty()) {
        return "opt_out";
    }
    if base_name(&profile.command) != "claude" {
        return "non_claude";
    }
    let _ = version;
    "unsupported_version"
}

/// Ensure the folder-trust prime has run once for `(profile identity, cwd)`.
///
/// Resolves the prime argv ([`routing::effective_preflight_args`]); an empty
/// argv is a safe skip. Otherwise serializes on the identity lock, checks the
/// persistent marker (claim-once — a prior `success` *or* `failed` means "do
/// not run again"), writes the deny settings file, runs the prime under
/// [`PREFLIGHT_TIMEOUT`], and records the outcome in the marker.
///
/// `config_dir` is the absolute `CLAUDE_CONFIG_DIR` the pane will use (issue
/// #235 f1), passed to the prime so both write/read folder trust in the same
/// place regardless of the mux server's own environment.
pub async fn ensure_preflight(
    profile: &AgentProfile,
    cwd: &Path,
    config_dir: &Path,
) -> PreflightOutcome {
    // The claude version only matters for the safe default (an explicit
    // `preflight` is used verbatim, and a non-`claude` command always skips), so
    // probe `--version` only then — never run `<command> --version` for a
    // command we would skip anyway (e.g. a fake agent in tests).
    let needs_version = profile.preflight.is_none() && base_name(&profile.command) == "claude";
    let version = if needs_version {
        detect_version(&profile.command).await
    } else {
        None
    };
    let deny_path = deny_settings_path();
    let argv = routing::effective_preflight_args(profile, version, &deny_path);
    if argv.is_empty() {
        return PreflightOutcome::Skipped {
            reason: skip_reason(profile, version),
        };
    }

    let key = identity_hash(&profile.command, config_dir, &argv, cwd);
    let marker = config::preflight_dir().join(&key);
    if marker.exists() {
        return PreflightOutcome::AlreadyDone;
    }

    let lock = lock_for(&key);
    let _guard = lock.lock().await;
    // Re-check under the lock: a concurrent first-spawn may have just primed.
    if marker.exists() {
        return PreflightOutcome::AlreadyDone;
    }

    // Set up the deny file only now that we are actually about to prime. If it
    // can't be written we must NOT run a tool-enabled turn — record a failure
    // (claim-once) and let the pane launch as today.
    if let Err(e) = ensure_deny_settings() {
        let reason = format!("deny-settings: {e:#}");
        write_marker(&marker, &format!("failed:{reason}"));
        return PreflightOutcome::Failed {
            reason,
            duration_ms: 0,
        };
    }

    let outcome = run_preflight(&profile.command, &argv, cwd, config_dir, PREFLIGHT_TIMEOUT).await;
    match &outcome {
        PreflightOutcome::Ran { .. } => write_marker(&marker, "success"),
        PreflightOutcome::Failed { reason, .. } => {
            write_marker(&marker, &format!("failed:{reason}"))
        }
        // run_preflight only ever returns Ran/Failed.
        _ => {}
    }
    outcome
}

fn write_marker(marker: &Path, record: &str) {
    // Best-effort: a marker we cannot write just means a possible redundant
    // re-prime later, never a correctness problem.
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker, record);
}

async fn detect_version(command: &str) -> Option<(u64, u64, u64)> {
    let command = command.to_string();
    tokio::task::spawn_blocking(move || routing::cli_version(&command))
        .await
        .ok()
        .flatten()
}

/// Run one prime as a plain async subprocess (no PTY — `-p` exits on its own).
/// Spawned in its own process group so a timeout can `killpg` the whole tree,
/// including any MCP/tool descendants, without blocking the Tokio runtime
/// (issue #235 f4). Mirrors `src/refine.rs`'s async pattern.
pub async fn run_preflight(
    command: &str,
    argv: &[String],
    cwd: &Path,
    config_dir: &Path,
    timeout: Duration,
) -> PreflightOutcome {
    let start = Instant::now();
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(argv)
        .current_dir(cwd)
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return PreflightOutcome::Failed {
                reason: format!("spawn: {e}"),
                duration_ms: elapsed_ms(start),
            };
        }
    };
    let pid = child.id().map(|p| p as libc::pid_t);

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => PreflightOutcome::Ran {
            duration_ms: elapsed_ms(start),
        },
        Ok(Ok(status)) => PreflightOutcome::Failed {
            reason: format!("nonzero:{}", status.code().unwrap_or(-1)),
            duration_ms: elapsed_ms(start),
        },
        Ok(Err(e)) => PreflightOutcome::Failed {
            reason: format!("wait:{e}"),
            duration_ms: elapsed_ms(start),
        },
        Err(_) => {
            if let Some(pid) = pid {
                kill_and_reap(&mut child, pid).await;
            }
            PreflightOutcome::Failed {
                reason: "timeout".to_string(),
                duration_ms: elapsed_ms(start),
            }
        }
    }
}

/// Kill the timed-out prime and its whole process group, then reap it without
/// blocking the runtime indefinitely. Same deadline/retry/give-up semantics as
/// the doctor gate probe (`gate::kill_and_reap_with_deadline`), re-expressed in
/// async because the two use different process models — but the shared
/// `REAP_DEADLINE` keeps the bound identical: the prime never outlives its
/// timeout by more than `2 × REAP_DEADLINE` (issue #235 f5).
async fn kill_and_reap(child: &mut tokio::process::Child, pid: libc::pid_t) {
    if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    for attempt in 0..2 {
        if tokio::time::timeout(REAP_DEADLINE, child.wait())
            .await
            .is_ok()
        {
            return;
        }
        if attempt == 0 {
            // One direct-pid retry in case the group kill silently missed.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    // Still not reapable: give up rather than hang. `kill_on_drop` backstops
    // the leak when `child` drops.
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_settings_is_valid_json_and_denies_every_surface() {
        let v: serde_json::Value = serde_json::from_str(DENY_SETTINGS_JSON).unwrap();
        let deny = v["permissions"]["deny"].as_array().unwrap();
        let names: Vec<&str> = deny.iter().map(|d| d.as_str().unwrap()).collect();
        for tool in ["Bash", "Read", "Edit", "Write", "WebFetch", "mcp__*"] {
            assert!(names.contains(&tool), "deny list missing {tool}");
        }
        assert_eq!(v["permissions"]["defaultMode"], "plan");
        assert_eq!(v["enableAllProjectMcpServers"], false);
    }

    #[cfg(unix)]
    #[test]
    fn write_deny_settings_is_0600_and_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = write_deny_settings_in(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), DENY_SETTINGS_JSON);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "deny.json must be owner-only");
        // Second call is a no-op that still returns the same path.
        let p2 = write_deny_settings_in(dir.path()).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn identity_hash_is_stable_and_path_sensitive() {
        let cd = Path::new("/cfg");
        let argv = vec!["-p".to_string(), "x".to_string()];
        let a = identity_hash("claude", cd, &argv, Path::new("/wt/one"));
        let a2 = identity_hash("claude", cd, &argv, Path::new("/wt/one"));
        let b = identity_hash("claude", cd, &argv, Path::new("/wt/two"));
        assert_eq!(a, a2, "same inputs → same hash");
        assert_ne!(a, b, "different target path → different hash");
    }

    #[test]
    fn skip_reason_classifies_opt_out_and_non_claude() {
        let opt_out = AgentProfile {
            command: "claude".into(),
            preflight: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(skip_reason(&opt_out, None), "opt_out");
        let codex = AgentProfile {
            command: "codex".into(),
            ..Default::default()
        };
        assert_eq!(skip_reason(&codex, Some((9, 0, 0))), "non_claude");
        let old_claude = AgentProfile {
            command: "claude".into(),
            ..Default::default()
        };
        assert_eq!(skip_reason(&old_claude, None), "unsupported_version");
    }

    #[tokio::test]
    async fn run_preflight_reports_success_and_nonzero() {
        let cwd = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let ok = run_preflight("true", &[], cwd.path(), cfg.path(), PREFLIGHT_TIMEOUT).await;
        assert!(matches!(ok, PreflightOutcome::Ran { .. }), "got {ok:?}");
        let bad = run_preflight("false", &[], cwd.path(), cfg.path(), PREFLIGHT_TIMEOUT).await;
        assert!(
            matches!(bad, PreflightOutcome::Failed { ref reason, .. } if reason.starts_with("nonzero")),
            "got {bad:?}"
        );
    }

    #[tokio::test]
    async fn run_preflight_times_out_and_reaps_within_bound() {
        let cwd = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let short = Duration::from_millis(150);
        let start = Instant::now();
        let out = run_preflight("sleep", &["30".to_string()], cwd.path(), cfg.path(), short).await;
        let elapsed = start.elapsed();
        assert!(
            matches!(out, PreflightOutcome::Failed { ref reason, .. } if reason == "timeout"),
            "got {out:?}"
        );
        // Never outlives timeout by more than 2×REAP_DEADLINE.
        assert!(
            elapsed < short + 2 * REAP_DEADLINE + Duration::from_secs(1),
            "prime hung too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn run_preflight_reports_spawn_failure_for_missing_command() {
        let cwd = tempfile::tempdir().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let out = run_preflight(
            "meguri-no-such-command-xyz",
            &[],
            cwd.path(),
            cfg.path(),
            PREFLIGHT_TIMEOUT,
        )
        .await;
        assert!(
            matches!(out, PreflightOutcome::Failed { ref reason, .. } if reason.starts_with("spawn")),
            "got {out:?}"
        );
    }
}
