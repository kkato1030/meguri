//! Daemon supervision tests: single-instance flock, state.json roundtrip,
//! status rendering, LaunchAgent plist generation, and platform gating.
//! Everything takes an explicit home dir, so no MEGURI_HOME env juggling.

use std::path::{Path, PathBuf};

use meguri::config::RestartPolicy;
use meguri::daemon::{self, DaemonState, WatchMode, launchd};

fn sample_state(pid: u32, mode: WatchMode, home: &Path) -> DaemonState {
    DaemonState {
        pid,
        mode,
        started_at: "2026-07-11T00:00:00Z".to_string(),
        version: "0.1.0".to_string(),
        log_path: mode.log_path(home),
    }
}

// --- flock -----------------------------------------------------------------

#[test]
fn lock_is_exclusive_and_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let lock = daemon::try_acquire_lock(dir.path()).unwrap();

    let err = daemon::try_acquire_lock(dir.path()).unwrap_err();
    assert!(err.to_string().contains("already running"), "{err}");

    drop(lock);
    daemon::try_acquire_lock(dir.path()).unwrap();
}

#[test]
fn second_start_reports_holder_pid_from_state() {
    let dir = tempfile::tempdir().unwrap();
    let _lock = daemon::try_acquire_lock(dir.path()).unwrap();
    daemon::write_state(
        dir.path(),
        &sample_state(4242, WatchMode::Detached, dir.path()),
    )
    .unwrap();

    let err = daemon::try_acquire_lock(dir.path()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("4242"), "{msg}");
    assert!(msg.contains("detached"), "{msg}");
}

// --- state.json ------------------------------------------------------------

#[test]
fn state_roundtrips_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let state = sample_state(123, WatchMode::Launchd, dir.path());
    daemon::write_state(dir.path(), &state).unwrap();
    assert_eq!(daemon::read_state(dir.path()).unwrap(), Some(state));
}

#[test]
fn state_absent_reads_as_none_and_clear_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(daemon::read_state(dir.path()).unwrap(), None);
    daemon::clear_state(dir.path());

    daemon::write_state(
        dir.path(),
        &sample_state(1, WatchMode::Foreground, dir.path()),
    )
    .unwrap();
    daemon::clear_state(dir.path());
    assert_eq!(daemon::read_state(dir.path()).unwrap(), None);
}

#[test]
fn mode_serializes_lowercase() {
    let dir = tempfile::tempdir().unwrap();
    daemon::write_state(
        dir.path(),
        &sample_state(7, WatchMode::Detached, dir.path()),
    )
    .unwrap();
    let raw = std::fs::read_to_string(daemon::state_path(dir.path())).unwrap();
    assert!(raw.contains("\"detached\""), "{raw}");
}

#[test]
fn log_path_is_per_mode() {
    let home = Path::new("/home/x/.meguri");
    assert_eq!(WatchMode::Foreground.log_path(home), None);
    assert_eq!(
        WatchMode::Detached.log_path(home),
        Some(PathBuf::from("/home/x/.meguri/logs/watch.log"))
    );
    assert_eq!(
        WatchMode::Launchd.log_path(home),
        Some(PathBuf::from("/home/x/.meguri/logs/launchd.log"))
    );
}

// --- status rendering ------------------------------------------------------

#[test]
fn status_not_running_without_state() {
    assert_eq!(
        daemon::status_report(None, false, None),
        "meguri watch: not running\n"
    );
}

#[test]
fn status_flags_stale_state_when_pid_is_dead() {
    let dir = tempfile::tempdir().unwrap();
    let state = sample_state(999_999, WatchMode::Detached, dir.path());
    let out = daemon::status_report(Some(&state), false, None);
    assert!(out.contains("not running"), "{out}");
    assert!(out.contains("stale"), "{out}");
    assert!(out.contains("999999"), "{out}");
}

#[test]
fn status_running_shows_pid_mode_log_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let state = sample_state(4321, WatchMode::Launchd, dir.path());
    let out = daemon::status_report(Some(&state), true, Some(2));
    assert!(out.contains("running"), "{out}");
    assert!(out.contains("4321"), "{out}");
    assert!(out.contains("launchd"), "{out}");
    assert!(out.contains("launchd.log"), "{out}");
    assert!(out.contains("active runs: 2"), "{out}");
}

#[test]
fn status_foreground_logs_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let state = sample_state(1, WatchMode::Foreground, dir.path());
    let out = daemon::status_report(Some(&state), true, None);
    assert!(out.contains("(stderr)"), "{out}");
}

// --- plist generation ------------------------------------------------------

fn sample_env() -> Vec<(String, String)> {
    vec![
        ("MEGURI_SUPERVISED".to_string(), "launchd".to_string()),
        ("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string()),
    ]
}

fn plist(policy: RestartPolicy, throttle: u64) -> String {
    launchd::render_plist(
        Path::new("/usr/local/bin/meguri"),
        policy,
        throttle,
        Path::new("/Users/x/.meguri/logs/launchd.log"),
        &sample_env(),
    )
}

#[test]
fn plist_on_failure_maps_to_successful_exit_false() {
    let p = plist(RestartPolicy::OnFailure, 10);
    assert!(p.contains("<key>KeepAlive</key>"), "{p}");
    assert!(p.contains("<key>SuccessfulExit</key>"), "{p}");
    assert!(p.contains("<false/>"), "{p}");
}

#[test]
fn plist_always_maps_to_keepalive_true() {
    let p = plist(RestartPolicy::Always, 10);
    assert!(p.contains("<key>KeepAlive</key>\n\t<true/>"), "{p}");
    assert!(!p.contains("SuccessfulExit"), "{p}");
}

#[test]
fn plist_never_has_run_at_load_but_no_keepalive() {
    let p = plist(RestartPolicy::Never, 10);
    assert!(p.contains("<key>RunAtLoad</key>"), "{p}");
    assert!(!p.contains("KeepAlive"), "{p}");
}

#[test]
fn plist_bakes_program_throttle_env_and_logs() {
    let p = plist(RestartPolicy::OnFailure, 42);
    assert!(p.contains("<string>dev.meguri.watch</string>"), "{p}");
    assert!(p.contains("<string>/usr/local/bin/meguri</string>"), "{p}");
    assert!(p.contains("<string>watch</string>"), "{p}");
    assert!(
        p.contains("<key>ThrottleInterval</key>\n\t<integer>42</integer>"),
        "{p}"
    );
    assert!(
        p.contains("<string>/opt/homebrew/bin:/usr/bin</string>"),
        "{p}"
    );
    assert!(p.contains("<key>MEGURI_SUPERVISED</key>"), "{p}");
    assert!(
        p.contains(
            "<key>StandardOutPath</key>\n\t<string>/Users/x/.meguri/logs/launchd.log</string>"
        ),
        "{p}"
    );
}

#[test]
fn plist_escapes_xml_in_env_values() {
    let env = vec![("PATH".to_string(), "/a&b/<bin>".to_string())];
    let p = launchd::render_plist(
        Path::new("/bin/meguri"),
        RestartPolicy::OnFailure,
        10,
        Path::new("/l.log"),
        &env,
    );
    assert!(p.contains("/a&amp;b/&lt;bin&gt;"), "{p}");
}

// --- platform gate ---------------------------------------------------------

#[test]
fn unsupported_mode_is_explicit_error() {
    let err = launchd::validate_mode("systemd").unwrap_err();
    assert!(err.to_string().contains("unsupported daemon mode"), "{err}");
}

#[cfg(target_os = "macos")]
#[test]
fn launchd_mode_is_accepted_on_macos() {
    launchd::validate_mode("launchd").unwrap();
}

#[cfg(not(target_os = "macos"))]
#[test]
fn launchd_mode_is_explicit_error_off_macos() {
    let err = launchd::validate_mode("launchd").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("macOS"), "{msg}");
    assert!(msg.contains("no supervisor"), "{msg}");
}
