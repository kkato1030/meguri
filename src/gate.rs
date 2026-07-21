//! Bypass-permissions gate probe (issue #234).
//!
//! `routing::probe_profile` fires the `claude` CLI headless (`-p`), so it
//! never reaches the interactive "Bypass Permissions mode" acceptance dialog
//! a live pane hits on its first launch against a given config dir. A doctor
//! that only runs the headless probe is a false-green oracle: it can report
//! ✅ while every real pane-launched turn stalls at that dialog waiting for a
//! human.
//!
//! This module runs a short, non-interactive PTY launch (no `-p`, same argv
//! a pane would use) of every profile a *pane*-launched role can actually
//! reach, and classifies the captured screen against known dialog/ready
//! text. It never reads `~/.claude.json`'s internal fields (as version-
//! fragile as writing them would be — same reasoning as the headless probe's
//! own doc), never answers the dialog (persisted acceptance state is never
//! touched), and never logs the captured terminal buffer.

use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, LaunchMode};
use crate::{launch, routing};

/// Doctor's severity for a bypass-gate probe. Distinct from
/// [`routing::ProbeOutcome`] (the headless model-alias probe): this
/// classifies the *interactive* first-launch gate the headless probe never
/// reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// The CLI reached its normal ready state — the bypass gate is already
    /// accepted for this config dir.
    Clear,
    /// The CLI is sitting at the known bypass-acceptance dialog — a live
    /// pane launch would stall here too, waiting for a human.
    Blocked,
    /// Timeout, unrecognized screen text, or a spawn failure: none of these
    /// is evidence the gate is clear, so doctor must never call it green.
    Inconclusive,
}

/// One profile identity to gate-probe: the argv/config-dir a `Pane`-launched
/// role would actually reach (issue #234, parent spec D2), minus the
/// trailing prompt trigger — the probe only wants to observe the startup
/// screen, never to hand it a real turn. `labels` lists every `role
/// (profile)` pair that shares this identity (deduped by `(command,
/// config_dir, args)` so the same CLI/config-dir pair is never launched
/// twice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateTarget {
    pub labels: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub config_dir: PathBuf,
}

/// The set of `Pane`-launched, non-`refiner` profiles to gate-probe (parent
/// spec D2): for each of the 6 routing roles, skip anything whose launch
/// mode ([`launch::resolve`]) isn't `Pane` (`self-reviewer`/`cleaner` are
/// `Direct` by default) and skip `refiner` outright — `launch::
/// recommended_mode` defaults it to `Pane` (`src/launch.rs:33`), but it is
/// actually `meguri add`'s headless-only refine (`effective_headless_args`),
/// never `spawn_agent_pane`; gate-probing it interactively would warn/fail on
/// a stall that can never happen. `detect` is the same CLI-detection closure
/// `routing::resolve` takes, injected so this stays testable without
/// spawning real subprocesses.
pub fn pane_gate_targets(cfg: &Config, detect: &dyn Fn(&str) -> bool) -> Vec<GateTarget> {
    let mut targets: Vec<GateTarget> = Vec::new();
    for role in routing::KNOWN_ROLES {
        if *role == "refiner" {
            continue;
        }
        if launch::resolve(cfg, role) != LaunchMode::Pane {
            continue;
        }
        let Ok(profile_name) = routing::resolve(cfg, role, detect) else {
            continue;
        };
        let Ok(profile) = routing::profile_by_name(cfg, &profile_name) else {
            continue;
        };
        let config_dir = pane_effective_config_dir();
        let label = format!("{role} ({profile_name})");
        match targets.iter_mut().find(|t| {
            t.command == profile.command && t.args == profile.args && t.config_dir == config_dir
        }) {
            Some(existing) => existing.labels.push(label),
            None => targets.push(GateTarget {
                labels: vec![label],
                command: profile.command.clone(),
                args: profile.args.clone(),
                config_dir,
            }),
        }
    }
    targets
}

/// The config-dir a real `Pane` launch actually resolves to (mirrors
/// `spawn_agent_pane`, `src/engine/flow.rs`): the pane inherits whatever
/// `$CLAUDE_CONFIG_DIR` the meguri process itself has (or the CLI's own
/// `~/.claude` default) — it is NOT redirected by `AgentProfile::
/// session_dir`. That field only steers where the *reaper* looks for a
/// resumable session id after the fact ([`crate::agent_session::
/// session_root`]); it plays no part in the actual spawn. Probing a
/// different dir than the one the pane will really use would make the
/// verdict meaningless (issue #234 self-review f2), so this resolves the
/// same way spawning does, not the reaper's lookup path — and deliberately
/// ignores the profile entirely, since none of it affects this.
///
/// Known gap (self-review f7, waived — see `.meguri/self-review-fix.json`):
/// this reads *doctor's own* environment and cwd, but a real pane is
/// launched by `meguri watch` through an already-running tmux/herdr server,
/// whose captured environment (and the `PaneSpec` worktree cwd) can differ
/// from doctor's — e.g. a `$CLAUDE_CONFIG_DIR` exported after the mux server
/// started, or a relative `$CLAUDE_CONFIG_DIR` (resolved against different
/// cwds by doctor vs. the pane). Same shape of gap as this issue's D1 (the
/// probe runs before the worktree exists and inherits doctor's cwd, not the
/// real launch's), which the parent spec already deferred to a future
/// launch-time pre-flight check rather than doctor. Closing it here would
/// mean either querying the mux server's own environment (backend-specific,
/// and still wouldn't cover doctor-process vs. watch-daemon-process drift)
/// or teaching `spawn_agent_pane` to pass an explicit env doctor cannot see
/// in advance — both bigger than this probe.
fn pane_effective_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

/// A one-line, actionable remediation for a `Blocked` target: accept the
/// dialog once by hand, and it stays accepted for every future pane launch
/// against the same config dir. Deliberately omits `target.args` — a
/// profile's launch args are user-configured and may carry a secret (an API
/// key, a bearer token, ...); echoing them into doctor's stdout / CI logs
/// would leak it (issue #234 self-review f4). Naming the command and
/// config-dir is enough to act on.
pub fn remediation_line(target: &GateTarget) -> String {
    format!(
        "一度 `{}` をそのプロファイルの起動引数で対話起動し、bypass permissions ダイアログを受諾して \
         ください（config-dir: {} に保存されます。起動引数は伏せています — profile 設定を確認してください）",
        target.command,
        target.config_dir.display(),
    )
}

/// Known screen text for the two decisive outcomes. Both are fragile to CLI
/// wording changes (same risk the headless probe's model-rejection text
/// carries) — the failure-side rule is what makes that safe: neither list
/// matching is never green, only `Clear` is (see [`classify_output`]).
const BYPASS_GATE_MARKERS: &[&str] = &["bypass permissions mode", "yes, i accept"];
const READY_MARKERS: &[&str] = &["welcome to claude code", "? for shortcuts"];

/// Classify a captured screen: a known bypass-gate marker wins (defensive —
/// never green if the two ever both matched), then a known ready marker,
/// else `Inconclusive`. A wording change that stops matching either list
/// falls to `Inconclusive`, never silently back to `Clear` — the same
/// false-green this module exists to close.
fn classify_output(text: &str) -> GateOutcome {
    let lower = text.to_lowercase();
    if BYPASS_GATE_MARKERS.iter().any(|m| lower.contains(m)) {
        return GateOutcome::Blocked;
    }
    if READY_MARKERS.iter().any(|m| lower.contains(m)) {
        return GateOutcome::Clear;
    }
    GateOutcome::Inconclusive
}

/// What the injected PTY launcher reports back to [`probe_gate`] — kept
/// distinct from [`GateOutcome`] so timeout / spawn-failure are testable as
/// their own seams, both collapsing to `Inconclusive`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyCapture {
    /// Screen text captured before a decisive marker matched, the process
    /// exited on its own, or the read hit an error/EOF.
    Output(String),
    /// No decisive marker appeared within the probe's deadline.
    Timeout,
    /// The PTY or the child process could not be spawned at all.
    SpawnFailed,
}

/// Classify one target via an injected PTY launcher — the seam
/// (`current closure 注入流儀`): production wires [`spawn_pty_probe`]; tests
/// inject a closure that returns each [`PtyCapture`] variant directly, no
/// subprocess involved.
pub fn probe_gate(target: &GateTarget, launch: &dyn Fn(&GateTarget) -> PtyCapture) -> GateOutcome {
    match launch(target) {
        PtyCapture::Output(text) => classify_output(&text),
        PtyCapture::Timeout | PtyCapture::SpawnFailed => GateOutcome::Inconclusive,
    }
}

/// How long the gate probe waits for a decisive screen before giving up.
/// Classification exits early once decisive (see
/// [`read_until_decisive_or_timeout`]), so a CLI that is already past the
/// gate returns almost immediately; this bound only matters for a CLI that
/// hangs or produces nothing recognizable.
pub const GATE_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Cap on captured PTY output so a chatty or runaway process can't grow the
/// buffer unbounded before the deadline.
const CAPTURE_CAP: usize = 64 * 1024;

/// Production PTY launcher for [`probe_gate`]: spawn the target under a
/// fresh PTY with no `-p` (the same argv a pane launch would use, minus the
/// trigger), read its screen for up to [`GATE_PROBE_TIMEOUT`], then kill and
/// reap the whole process group — an interactive CLI does not exit on its
/// own, and its descendants (if any) must not be left running. Never writes
/// to the PTY: no simulated keystrokes, so the dialog's persisted acceptance
/// state is untouched either way.
pub fn spawn_pty_probe(target: &GateTarget) -> PtyCapture {
    spawn_pty_probe_with_timeout(target, GATE_PROBE_TIMEOUT)
}

/// Same as [`spawn_pty_probe`] with an explicit deadline — a thin seam of
/// its own so integration tests can exercise the real timeout path (process
/// spawn, PTY read, process-group kill) without waiting the full production
/// bound.
pub fn spawn_pty_probe_with_timeout(target: &GateTarget, timeout: Duration) -> PtyCapture {
    match spawn_pty_probe_inner(target, timeout) {
        Ok(capture) => capture,
        Err(_) => PtyCapture::SpawnFailed,
    }
}

struct FdGuard(RawFd);

impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// `ptsname` writes into a shared, process-wide static buffer — not
/// reentrant. Doctor's own probe loop runs targets one at a time, but the
/// test suite doesn't: plain `cargo test` (unlike `cargo nextest`) runs every
/// `#[test]` fn in one process on separate threads, and two gate probes
/// racing here corrupted each other's slave path and hung for 60s+ (issue
/// #234 self-review f3). Serialize the whole acquire sequence — open through
/// `ptsname` — behind this lock; the read/timeout phase afterward stays
/// concurrent, each probe on its own fd.
static PTY_ACQUIRE_LOCK: Mutex<()> = Mutex::new(());

/// Open a PTY master/slave pair and return `(master fd, slave path)`, holding
/// [`PTY_ACQUIRE_LOCK`] for the whole sequence (see its doc).
fn acquire_pty() -> Result<(RawFd, PathBuf)> {
    let _guard = match PTY_ACQUIRE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        bail!("posix_openpt failed: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::grantpt(fd) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        bail!("grantpt failed: {err}");
    }
    if unsafe { libc::unlockpt(fd) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        bail!("unlockpt failed: {err}");
    }

    let ptr = unsafe { libc::ptsname(fd) };
    if ptr.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        bail!("ptsname failed: {err}");
    }
    // Copy out of the shared buffer before the lock (and thus the next
    // caller's ptsname call) is released.
    let slave_path = PathBuf::from(
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    );

    Ok((fd, slave_path))
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        bail!("fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        bail!("fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn spawn_pty_probe_inner(target: &GateTarget, timeout: Duration) -> Result<PtyCapture> {
    let (master, slave_path) = acquire_pty().context("acquire pty")?;
    let master_guard = FdGuard(master);

    let open_slave = || -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&slave_path)
    };
    let stdin = open_slave().with_context(|| format!("open pty slave {}", slave_path.display()))?;
    let stdout =
        open_slave().with_context(|| format!("open pty slave {}", slave_path.display()))?;
    let stderr =
        open_slave().with_context(|| format!("open pty slave {}", slave_path.display()))?;

    let mut cmd = std::process::Command::new(&target.command);
    // No explicit CLAUDE_CONFIG_DIR: the child inherits our env exactly like
    // a real pane spawn does (`pane_effective_config_dir` computes
    // `target.config_dir` the same way, without redirecting anything here —
    // see its doc / issue #234 self-review f2).
    cmd.args(&target.args)
        .env("TERM", "xterm-256color")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    {
        // New process group (pgid = the child's own pid), so a single
        // killpg(pid) reaps it and every descendant. `process_group(0)`
        // (stable API, no `pre_exec`/`unsafe` needed) lets std use
        // `posix_spawn` instead of `fork`+exec — `pre_exec` forces the
        // classic fork path, and forking this process (which links tokio
        // and other multi-threaded runtime bits) deadlocked the child
        // inside its own exec on this machine (issue #234 self-review f3
        // follow-up: the observed 60s+ hang wasn't only the `ptsname` race,
        // it also reproduced with a single, fully serialized probe —
        // `fork()` after multiple threads exist is the classic culprit).
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Give the PTY a real window size before the CLI starts: a 0×0 window
    // makes some TUIs skip rendering entirely, which would burn the whole
    // per-target timeout as a silent `Inconclusive`.
    let winsize = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &winsize) };

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(PtyCapture::SpawnFailed),
    };
    let pid = child.id() as libc::pid_t;

    set_nonblocking(master)?;
    let capture = read_until_decisive_or_timeout(master, timeout);

    kill_and_reap_with_deadline(&mut child, pid);
    drop(master_guard);

    Ok(capture)
}

/// How long to wait for the killed probe child to be reapable before giving
/// up and leaking it as a zombie. Giving up is deliberate: an unbounded
/// `wait()` here would let one unkillable child hang `doctor --probe` past
/// its declared per-target timeout forever.
const REAP_DEADLINE: Duration = Duration::from_secs(2);

/// Kill the probe child and reap it without ever blocking indefinitely.
///
/// `killpg(pid)` is the happy path (the child was spawned as its own group
/// leader, so the whole tree dies), but it is not guaranteed to deliver: the
/// CLI may have moved itself to another process group, or the group may be
/// gone already (ESRCH). Falling back to `kill(pid)` and reaping via
/// `try_wait` under a deadline keeps the invariant that a probe never
/// outlives its declared timeout by more than [`REAP_DEADLINE`].
fn kill_and_reap_with_deadline(child: &mut std::process::Child, pid: libc::pid_t) {
    // Expected to hit ESRCH once in a while if the child died on its own;
    // the direct-pid fallback covers a child that left its process group.
    if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let mut deadline = Instant::now() + REAP_DEADLINE;
    let mut resignaled = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            if resignaled {
                // Still not reapable: leak the zombie rather than hang the
                // probe (the doctor process is short-lived anyway).
                return;
            }
            // One direct-pid retry (with a second reap window) in case the
            // group kill silently missed.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            resignaled = true;
            deadline = Instant::now() + REAP_DEADLINE;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How long to keep reading after a ready marker first appears, before
/// finalizing as `Clear`. A CLI can render its ready banner and then, a
/// moment later, still show the bypass dialog (e.g. a slow first-run
/// migration between the two) — returning the instant `Clear` looked true
/// would false-green exactly the race this module exists to close (issue
/// #234 self-review f1). A `Blocked` marker arriving during this window
/// still wins immediately (see the loop below); only the absence of one
/// for the whole window finalizes `Clear`.
const READY_SETTLE_WINDOW: Duration = Duration::from_millis(1500);

fn read_until_decisive_or_timeout(master: RawFd, timeout: Duration) -> PtyCapture {
    let hard_deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    // Set the first time a ready marker is seen; None means "no ready marker
    // yet, the hard deadline is a plain Timeout".
    let mut settle_deadline: Option<Instant> = None;

    loop {
        let now = Instant::now();
        let effective_deadline = match settle_deadline {
            Some(t) => t.min(hard_deadline),
            None => hard_deadline,
        };
        if now >= effective_deadline {
            return if settle_deadline.is_some() {
                PtyCapture::Output(String::from_utf8_lossy(&buf).into_owned())
            } else {
                PtyCapture::Timeout
            };
        }
        let remaining = effective_deadline - now;
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                // EINTR (e.g. SIGWINCH) is not evidence of anything — retry
                // rather than cutting the settle window short and false-
                // greening a bypass dialog that was about to appear (issue
                // #234 self-review f6).
                continue;
            }
            return if settle_deadline.is_some() {
                PtyCapture::Output(String::from_utf8_lossy(&buf).into_owned())
            } else {
                PtyCapture::Timeout
            };
        }
        if n == 0 {
            continue;
        }
        let r = unsafe { libc::read(master, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if r > 0 {
            let r = r as usize;
            let room = CAPTURE_CAP.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..r.min(room)]);
            let text = String::from_utf8_lossy(&buf).into_owned();
            match classify_output(&text) {
                // Decisive and safety-first: stop immediately, never wait
                // out a settle window for a Blocked verdict.
                GateOutcome::Blocked => return PtyCapture::Output(text),
                // Decisive-looking but not final: start (or keep) the settle
                // window instead of returning, so a later Blocked marker
                // still wins.
                GateOutcome::Clear => {
                    settle_deadline.get_or_insert_with(|| Instant::now() + READY_SETTLE_WINDOW);
                }
                GateOutcome::Inconclusive => {}
            }
        } else if r == 0 {
            // EOF: the slave side is gone (process exited on its own) —
            // nothing more can arrive, so finalize on whatever we have.
            return PtyCapture::Output(String::from_utf8_lossy(&buf).into_owned());
        } else {
            let err = std::io::Error::last_os_error();
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                // WouldBlock: poll raced the data away; Interrupted: a signal
                // landed mid-read — neither says anything about the child, so
                // retry instead of finalizing early (a false EOF here could
                // false-green a bypass dialog that was about to appear).
                continue;
            }
            // Most commonly EIO once the slave closes — treat like EOF.
            return PtyCapture::Output(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    fn only(available: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |cmd: &str| available.contains(&cmd)
    }

    // --- classify_output / probe_gate seam ---------------------------------

    #[test]
    fn classify_detects_known_bypass_gate_wording_as_blocked() {
        let screen = "WARNING: Claude Code running in Bypass Permissions mode\n\
             1. No, exit\n2. Yes, I accept";
        assert_eq!(classify_output(screen), GateOutcome::Blocked);
    }

    #[test]
    fn classify_detects_ready_wording_as_clear() {
        let screen = "✻ Welcome to Claude Code!\n\n> \n  ? for shortcuts";
        assert_eq!(classify_output(screen), GateOutcome::Clear);
    }

    #[test]
    fn classify_falls_back_to_inconclusive_on_unknown_text() {
        // A wording change that stops matching either list must never be
        // silently treated as Clear (the false-green this module exists to
        // close).
        assert_eq!(
            classify_output("some future banner nobody has seen yet"),
            GateOutcome::Inconclusive
        );
        assert_eq!(classify_output(""), GateOutcome::Inconclusive);
    }

    #[test]
    fn classify_prefers_blocked_when_both_markers_somehow_match() {
        let screen = "welcome to claude code\nBypass Permissions mode\nyes, I accept";
        assert_eq!(classify_output(screen), GateOutcome::Blocked);
    }

    #[test]
    fn probe_gate_maps_gate_wording_to_blocked() {
        let target = fake_target();
        let launch = |_: &GateTarget| {
            PtyCapture::Output("Bypass Permissions mode\n2. Yes, I accept".to_string())
        };
        assert_eq!(probe_gate(&target, &launch), GateOutcome::Blocked);
    }

    #[test]
    fn probe_gate_maps_ready_wording_to_clear() {
        let target = fake_target();
        let launch = |_: &GateTarget| PtyCapture::Output("Welcome to Claude Code!".to_string());
        assert_eq!(probe_gate(&target, &launch), GateOutcome::Clear);
    }

    #[test]
    fn probe_gate_maps_timeout_to_inconclusive() {
        let target = fake_target();
        let launch = |_: &GateTarget| PtyCapture::Timeout;
        assert_eq!(probe_gate(&target, &launch), GateOutcome::Inconclusive);
    }

    #[test]
    fn probe_gate_maps_spawn_failure_to_inconclusive() {
        let target = fake_target();
        let launch = |_: &GateTarget| PtyCapture::SpawnFailed;
        assert_eq!(probe_gate(&target, &launch), GateOutcome::Inconclusive);
    }

    fn fake_target() -> GateTarget {
        GateTarget {
            labels: vec!["worker (default)".to_string()],
            command: "fake-claude".to_string(),
            args: vec!["--dangerously-skip-permissions".to_string()],
            config_dir: PathBuf::from("/tmp/fake-claude-config"),
        }
    }

    // --- pane_gate_targets ---------------------------------------------------

    #[test]
    fn legacy_config_merges_every_pane_role_into_one_default_target() {
        // No [routing]/[launch]: every non-refiner role's recommended launch
        // mode is Pane and every role resolves to the same `default` profile
        // ([agent]), so they all collapse into a single gate target.
        let cfg = Config::default();
        let never = |_: &str| panic!("legacy resolve must not detect");
        let targets = pane_gate_targets(&cfg, &never);
        assert_eq!(targets.len(), 1);
        let t = &targets[0];
        assert_eq!(t.command, cfg.agent.command);
        assert_eq!(t.args, cfg.agent.args);
        // planner / worker / fixer / pr-reviewer are Pane by default;
        // self-reviewer / cleaner are Direct; refiner is excluded outright.
        assert_eq!(t.labels.len(), 4);
        for role in ["planner", "worker", "fixer", "pr-reviewer"] {
            assert!(
                t.labels.iter().any(|l| l.starts_with(role)),
                "missing {role} in {:?}",
                t.labels
            );
        }
        for role in ["self-reviewer", "cleaner", "refiner"] {
            assert!(
                t.labels.iter().all(|l| !l.starts_with(role)),
                "unexpected {role} in {:?}",
                t.labels
            );
        }
    }

    #[test]
    fn auto_routing_splits_targets_by_resolved_profile_and_dedups_shared_ones() {
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        let targets = pane_gate_targets(&cfg, &only(&["claude", "codex"]));
        // worker + fixer both land on claude-sonnet -> one shared target;
        // pr-reviewer lands on codex; planner lands on claude-opus.
        assert_eq!(targets.len(), 3);
        let sonnet = targets
            .iter()
            .find(|t| t.labels.iter().any(|l| l.starts_with("worker")))
            .expect("worker target present");
        assert!(sonnet.labels.iter().any(|l| l.starts_with("fixer")));
        assert_eq!(sonnet.labels.len(), 2);
    }

    #[test]
    fn refiner_is_never_probed_even_when_launch_mode_is_pane() {
        // refiner's recommended launch mode defaults to Pane
        // (launch::recommended_mode falls through to Pane for unknown/
        // unlisted roles), but it must never be gate-probed (parent spec
        // D2): it only ever runs headless via `meguri add`.
        let cfg = Config::default();
        assert_eq!(launch::resolve(&cfg, "refiner"), LaunchMode::Pane);
        let targets = pane_gate_targets(&cfg, &only(&["claude"]));
        assert!(
            targets
                .iter()
                .all(|t| t.labels.iter().all(|l| !l.starts_with("refiner"))),
            "{targets:?}"
        );
    }

    #[test]
    fn explicit_direct_launch_override_removes_a_role_from_the_gate_set() {
        let cfg = cfg_from("[launch.roles]\nworker = \"direct\"\n");
        let targets = pane_gate_targets(&cfg, &only(&["claude"]));
        assert!(
            targets
                .iter()
                .all(|t| t.labels.iter().all(|l| !l.starts_with("worker"))),
            "{targets:?}"
        );
        // planner is untouched and still probed.
        assert!(
            targets
                .iter()
                .any(|t| t.labels.iter().any(|l| l.starts_with("planner")))
        );
    }

    #[test]
    fn remediation_line_names_the_command_and_config_dir_but_never_the_args() {
        // A profile's launch args are user-configured and may carry a
        // secret; remediation_line must never echo them into doctor's
        // stdout / CI logs (issue #234 self-review f4).
        let t = GateTarget {
            labels: vec!["worker (default)".to_string()],
            command: "fake-claude".to_string(),
            args: vec!["--api-key".to_string(), "sk-super-secret-token".to_string()],
            config_dir: PathBuf::from("/tmp/fake-claude-config"),
        };
        let line = remediation_line(&t);
        assert!(line.contains("fake-claude"));
        assert!(line.contains("/tmp/fake-claude-config"));
        assert!(!line.contains("sk-super-secret-token"), "{line}");
        assert!(!line.contains("--api-key"), "{line}");
    }

    #[test]
    fn profiles_differing_only_by_session_dir_still_share_one_gate_target() {
        // AgentProfile::session_dir only steers the reaper's session-
        // transcript lookup (crate::agent_session::session_root); it plays
        // no part in what config-dir a real pane spawn actually uses, so two
        // profiles that differ only there must still probe as one identity
        // (issue #234 self-review f2) — probing them separately, or against
        // a dir the real launch never uses, would make the verdict
        // meaningless.
        // variant-a/b's args deliberately differ from the `default` profile's
        // (`--dangerously-skip-permissions` alone) so this test isolates the
        // session_dir question — planner/pr-reviewer (unlisted in manual
        // mode, so they fall to `default`) forming their own separate target
        // is expected and unrelated to what's under test here.
        let cfg = cfg_from(
            r#"
[routing]
mode = "manual"

[routing.roles]
worker = "variant-a"
fixer = "variant-b"

[agents.profiles.variant-a]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "sonnet"]
session_dir = "/tmp/custom-a"

[agents.profiles.variant-b]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "sonnet"]
session_dir = "/tmp/custom-b"
"#,
        );
        let targets = pane_gate_targets(&cfg, &only(&["claude"]));
        let variants = targets
            .iter()
            .find(|t| t.labels.iter().any(|l| l.starts_with("worker")))
            .expect("worker/fixer target present");
        assert_eq!(variants.labels.len(), 2, "{targets:?}");
        assert!(variants.labels.iter().any(|l| l.starts_with("fixer")));
    }
}
