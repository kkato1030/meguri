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
fn real_pty_probe_times_out_on_silence_and_reaps_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    // The grandchild is its own script file with a unique name, so pgrep -f
    // can find it independently of the parent — a fix that only kills the
    // direct child (and leaves this backgrounded grandchild behind) must
    // fail this assertion, not silently pass (issue #234 self-review f5:
    // the previous version only pgrep'd the parent's name, so a partial
    // reap wouldn't have been caught).
    let grandchild = fake_cli(
        dir.path(),
        "fake-claude-hang-grandchild",
        "while true; do sleep 1; done",
    );
    let cli = fake_cli(
        dir.path(),
        "fake-claude-hang-parent",
        &format!("{} &\nwhile true; do sleep 1; done", grandchild.display()),
    );
    let target = target_for(cli, dir.path());
    match spawn_pty_probe_with_timeout(&target, Duration::from_millis(500)) {
        PtyCapture::Timeout => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    // Give the OS a beat to finish tearing the tree down, then confirm
    // nothing matching our fake scripts — parent or grandchild — is left.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !any_process_matching("fake-claude-hang-parent"),
        "gate probe left the parent process behind after timeout"
    );
    assert!(
        !any_process_matching("fake-claude-hang-grandchild"),
        "gate probe left a backgrounded grandchild behind — process group not fully reaped"
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

#[test]
fn real_pty_probe_prefers_a_later_blocked_marker_over_an_earlier_ready_one() {
    // A CLI can render its ready banner first and only show the bypass
    // dialog a moment later (e.g. a slow first-run step in between) — the
    // probe must not declare Clear the instant "ready" appears (issue #234
    // self-review f1). The delay here is well inside gate::READY_SETTLE_
    // WINDOW (1.5s).
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_cli(
        dir.path(),
        "fake-claude-late-gate",
        r#"echo "Welcome to Claude Code!"
sleep 0.3
echo "WARNING: Claude Code running in Bypass Permissions mode"
echo "2. Yes, I accept"
while true; do sleep 1; done"#,
    );
    let target = target_for(cli, dir.path());
    assert_eq!(probe_gate(&target, &spawn_pty_probe), GateOutcome::Blocked);
}

#[test]
fn concurrent_real_pty_probes_do_not_corrupt_each_others_slave_path() {
    // `ptsname` writes into a shared static buffer; racing probes without
    // synchronizing the acquire sequence corrupted each other's slave path
    // and hung for 60s+ under plain `cargo test` (issue #234 self-review
    // f3, which unlike `cargo nextest` runs every #[test] fn in one process
    // on separate threads — this test adds its own explicit concurrency on
    // top of that, so it exercises the race even under nextest).
    let dir = tempfile::tempdir().unwrap();
    let gate_cli = fake_cli(
        dir.path(),
        "fake-claude-concurrent-gate",
        r#"echo "WARNING: Claude Code running in Bypass Permissions mode"
while true; do sleep 1; done"#,
    );
    let ready_cli = fake_cli(
        dir.path(),
        "fake-claude-concurrent-ready",
        r#"echo "Welcome to Claude Code!"
while true; do sleep 1; done"#,
    );

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let cli = if i % 2 == 0 { &gate_cli } else { &ready_cli };
                let expected = if i % 2 == 0 {
                    GateOutcome::Blocked
                } else {
                    GateOutcome::Clear
                };
                let target = target_for(cli.clone(), dir.path());
                scope.spawn(move || {
                    let outcome = probe_gate(&target, &spawn_pty_probe);
                    assert_eq!(outcome, expected, "probe {i} got a foreign/garbled slave");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("probe thread panicked");
        }
    });
}
