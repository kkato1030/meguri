//! Bypass-gate probe (issue #234) against fake interactive CLIs run under a
//! real PTY, so the spawn / read / classify / process-group-kill path is
//! exercised end-to-end — not just the classifier via an injected closure
//! (that's `src/gate.rs`'s own unit tests). Mirrors `doctor_probe_test.rs`'s
//! fake-`claude`-script convention.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use meguri::gate::{
    GateOutcome, GateTarget, PtyCapture, probe_gate, spawn_pty_probe, spawn_pty_probe_with_timeout,
};

/// Write an executable fake CLI script, return its path.
fn fake_cli(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, "#!/bin/sh\n{body}\n").unwrap();
    let mut perms = f.metadata().unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn target_for(command: PathBuf, config_dir: &Path) -> GateTarget {
    GateTarget {
        labels: vec!["worker (test)".into()],
        command: command.to_string_lossy().into_owned(),
        args: vec![],
        config_dir: config_dir.to_path_buf(),
    }
}

/// Whether any process is still running with `needle` in its argv — used to
/// assert the probe actually reaped the hung child (not just returned).
fn any_process_matching(needle: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", needle])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}

#[test]
fn real_pty_probe_detects_the_bypass_gate() {
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_cli(
        dir.path(),
        "fake-claude-gate",
        r#"echo "WARNING: Claude Code running in Bypass Permissions mode"
echo "2. Yes, I accept"
while true; do sleep 1; done"#,
    );
    let target = target_for(cli, dir.path());
    assert_eq!(probe_gate(&target, &spawn_pty_probe), GateOutcome::Blocked);
}

#[test]
fn real_pty_probe_recognizes_the_ready_state() {
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_cli(
        dir.path(),
        "fake-claude-ready",
        r#"echo "Welcome to Claude Code!"
while true; do sleep 1; done"#,
    );
    let target = target_for(cli, dir.path());
    assert_eq!(probe_gate(&target, &spawn_pty_probe), GateOutcome::Clear);
}

#[test]
fn real_pty_probe_times_out_on_silence_and_reaps_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_cli(
        dir.path(),
        "fake-claude-hang",
        // A child of its own too, so we assert the whole group is reaped —
        // not just the direct child.
        "sleep 60 & while true; do sleep 1; done",
    );
    let target = target_for(cli, dir.path());
    match spawn_pty_probe_with_timeout(&target, Duration::from_millis(500)) {
        PtyCapture::Timeout => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    // Give the OS a beat to finish tearing the tree down, then confirm
    // nothing matching our fake script is still running.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !any_process_matching("fake-claude-hang"),
        "gate probe left a process behind after timeout"
    );
}

#[test]
fn real_pty_probe_reports_spawn_failure_for_a_missing_command() {
    let dir = tempfile::tempdir().unwrap();
    let target = target_for(dir.path().join("does-not-exist"), dir.path());
    assert_eq!(
        probe_gate(&target, &spawn_pty_probe),
        GateOutcome::Inconclusive
    );
}
