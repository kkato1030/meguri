//! doctor's live-launch probe (routing 2/3, issue #65) against a fake `claude`
//! binary, so the model-alias classification is exercised end-to-end without
//! spending real quota. The fake is a shell script named `claude` whose
//! behavior we script per case.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use meguri::config::AgentProfile;
use meguri::routing::{ProbeOutcome, probe_profile};

/// Write an executable `claude` script emulating a CLI, return its path.
fn fake_claude(dir: &std::path::Path, body: &str) -> PathBuf {
    let path = dir.join("claude");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, "#!/bin/sh\n{body}\n").unwrap();
    let mut perms = f.metadata().unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn profile_for(command: PathBuf) -> AgentProfile {
    AgentProfile {
        command: command.to_string_lossy().into_owned(),
        args: vec!["--model".into(), "sonnet".into()],
        resume_args: vec![],
        direct_args: vec![],
        herdr_agent_hint: None,
        session_dir: None,
    }
}

#[test]
fn probe_flags_invalid_model_as_model_invalid() {
    let dir = tempfile::tempdir().unwrap();
    // Exit non-zero with a model-rejection message on stderr.
    let claude = fake_claude(
        dir.path(),
        r#"echo "error: invalid model: sonnet is unknown" 1>&2
exit 1"#,
    );
    assert_eq!(
        probe_profile(&profile_for(claude)),
        ProbeOutcome::ModelInvalid
    );
}

#[test]
fn probe_treats_network_failure_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    // Exit non-zero but with a transport error, not a model error.
    let claude = fake_claude(
        dir.path(),
        r#"echo "error: connection reset by peer" 1>&2
exit 1"#,
    );
    assert_eq!(
        probe_profile(&profile_for(claude)),
        ProbeOutcome::Unavailable
    );
}

#[test]
fn probe_ok_when_cli_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let claude = fake_claude(dir.path(), r#"echo ok"#);
    assert_eq!(probe_profile(&profile_for(claude)), ProbeOutcome::Ok);
}
