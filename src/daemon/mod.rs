//! Daemonize `meguri watch`: detached spawn, single-instance flock,
//! supervision state, and the `meguri daemon` verbs.
//!
//! No CLI↔daemon IPC (ADR 0001): `daemon status` answers from a state file
//! plus a liveness probe, and the single-instance guarantee is an exclusive
//! flock held by the watch process itself — never a stale-prone pidfile.

pub mod launchd;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};

/// Set by the detach spawner / launchd plist so the watch process can record
/// how it is supervised; absent means an interactive foreground `meguri watch`.
pub const SUPERVISED_ENV: &str = "MEGURI_SUPERVISED";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    Foreground,
    Detached,
    Launchd,
}

impl WatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Detached => "detached",
            Self::Launchd => "launchd",
        }
    }

    pub fn from_env() -> Self {
        match std::env::var(SUPERVISED_ENV).as_deref() {
            Ok("detached") => Self::Detached,
            Ok("launchd") => Self::Launchd,
            _ => Self::Foreground,
        }
    }

    /// Where this mode's stdout/stderr land (foreground stays on the tty).
    pub fn log_path(self, home: &Path) -> Option<PathBuf> {
        match self {
            Self::Foreground => None,
            Self::Detached => Some(logs_dir(home).join("watch.log")),
            Self::Launchd => Some(logs_dir(home).join("launchd.log")),
        }
    }
}

pub fn daemon_dir(home: &Path) -> PathBuf {
    home.join("daemon")
}

pub fn logs_dir(home: &Path) -> PathBuf {
    home.join("logs")
}

pub fn lock_path(home: &Path) -> PathBuf {
    daemon_dir(home).join("watch.lock")
}

pub fn state_path(home: &Path) -> PathBuf {
    daemon_dir(home).join("state.json")
}

/// Supervision metadata the watch process writes at startup (`state.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub mode: WatchMode,
    pub started_at: String,
    pub version: String,
    pub log_path: Option<PathBuf>,
}

impl DaemonState {
    /// State for the calling process running `watch` in `mode`.
    pub fn for_current_process(home: &Path, mode: WatchMode) -> Self {
        Self {
            pid: std::process::id(),
            mode,
            started_at: crate::store::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            log_path: mode.log_path(home),
        }
    }
}

/// Exclusive flock on `daemon/watch.lock`; the watch process holds it for its
/// whole lifetime, so the OS releases it on any exit — no stale-lock cleanup.
#[derive(Debug)]
pub struct WatchLock {
    _file: File,
}

pub fn try_acquire_lock(home: &Path) -> Result<WatchLock> {
    let path = lock_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(WatchLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => match read_state(home).ok().flatten() {
            Some(state) => bail!(
                "meguri watch is already running (pid {}, mode {}) — see `meguri daemon status`",
                state.pid,
                state.mode.as_str()
            ),
            None => bail!("meguri watch is already running — see `meguri daemon status`"),
        },
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

pub fn write_state(home: &Path, state: &DaemonState) -> Result<()> {
    let path = state_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("cannot write {}", path.display()))
}

pub fn read_state(home: &Path) -> Result<Option<DaemonState>> {
    match std::fs::read_to_string(state_path(home)) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn clear_state(home: &Path) {
    let _ = std::fs::remove_file(state_path(home));
}

/// `kill(pid, 0)` liveness probe (same-UID, so EPERM cannot mislead us).
pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Render `daemon status` from the state file + liveness + sqlite (pure, for
/// tests); the launchd supervisor excerpt is appended separately.
pub fn status_report(
    state: Option<&DaemonState>,
    alive: bool,
    active_runs: Option<usize>,
) -> String {
    let Some(state) = state else {
        return "meguri watch: not running\n".to_string();
    };
    if !alive {
        return format!(
            "meguri watch: not running (stale state: pid {} is dead)\n",
            state.pid
        );
    }
    let mut out = String::from("meguri watch: running\n");
    out += &format!("  pid:         {}\n", state.pid);
    out += &format!("  mode:        {}\n", state.mode.as_str());
    out += &format!("  started at:  {}\n", state.started_at);
    out += &format!("  version:     {}\n", state.version);
    out += &format!(
        "  log:         {}\n",
        state
            .log_path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(stderr)".to_string())
    );
    if let Some(n) = active_runs {
        out += &format!("  active runs: {n}\n");
    }
    out
}

/// `meguri daemon start`: spawn `meguri watch` detached (setsid, /dev/null
/// stdin, log-file stdout/stderr) and return once the child has settled.
pub fn cmd_start() -> Result<()> {
    let home = config::meguri_home();

    // Fail fast while we can still print to the user's terminal; the child
    // re-checks under its own flock, so this early probe is best-effort only.
    drop(try_acquire_lock(&home)?);
    let cfg = Config::load()?;
    if cfg.projects.is_empty() {
        bail!(
            "no projects configured — edit {}",
            config::config_path().display()
        );
    }

    let exe = std::env::current_exe().context("cannot resolve the meguri executable path")?;
    let log_path = WatchMode::Detached
        .log_path(&home)
        .expect("detached mode always has a log path");
    std::fs::create_dir_all(logs_dir(&home))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("watch")
        .env(SUPERVISED_ENV, "detached")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // New session: no controlling terminal, so closing the shell
            // cannot SIGHUP the watch.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("cannot spawn detached watch")?;
    let pid = child.id();

    // Wait briefly for the child to take the flock and write state, so an
    // immediate startup failure surfaces here instead of only in the log.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(status) = child.try_wait()? {
            bail!(
                "watch exited immediately ({status}) — check {}",
                log_path.display()
            );
        }
        if read_state(&home)
            .ok()
            .flatten()
            .is_some_and(|s| s.pid == pid)
        {
            break;
        }
    }
    println!("meguri watch detached (pid {pid})");
    println!("  log:  {}", log_path.display());
    println!("  stop: meguri daemon stop");
    Ok(())
}

/// `meguri daemon stop`: SIGTERM + state cleanup. No graceful shutdown —
/// meguri is kill-safe; the next start's recovery resumes interrupted runs.
/// In launchd mode this boots the job out so `KeepAlive` cannot resurrect it.
pub fn cmd_stop() -> Result<()> {
    let home = config::meguri_home();
    let Some(state) = read_state(&home)? else {
        println!("meguri watch: not running");
        return Ok(());
    };

    if state.mode == WatchMode::Launchd {
        launchd::bootout()?;
        println!(
            "booted {} out of launchd (auto-restart disabled until `meguri daemon install`)",
            launchd::LABEL
        );
    } else if pid_alive(state.pid) {
        unsafe {
            libc::kill(state.pid as libc::pid_t, libc::SIGTERM);
        }
        for _ in 0..50 {
            if !pid_alive(state.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(state.pid) {
            bail!("pid {} did not exit within 5s of SIGTERM", state.pid);
        }
        println!("stopped meguri watch (pid {})", state.pid);
    } else {
        println!(
            "meguri watch (pid {}) is already dead — cleaning up stale state",
            state.pid
        );
    }
    clear_state(&home);
    Ok(())
}

/// `meguri daemon restart`: stop + start, keeping the supervision mode.
pub fn cmd_restart() -> Result<()> {
    let home = config::meguri_home();
    match read_state(&home)?.map(|s| s.mode) {
        Some(WatchMode::Launchd) => {
            // launchd owns the process: kill-and-restart in place.
            launchd::kickstart()?;
            println!("kickstarted {}", launchd::LABEL);
            Ok(())
        }
        _ => {
            cmd_stop()?;
            cmd_start()
        }
    }
}

/// `meguri daemon status`: state file + `kill(pid, 0)` + sqlite, plus the
/// launchd supervisor's own view when relevant. No IPC (ADR 0001).
pub fn cmd_status() -> Result<()> {
    let home = config::meguri_home();
    let state = read_state(&home)?;
    let alive = state.as_ref().is_some_and(|s| pid_alive(s.pid));
    let active_runs = crate::store::Store::open(&config::db_path())
        .ok()
        .and_then(|s| s.list_runs(true).ok())
        .map(|runs| runs.len());
    print!("{}", status_report(state.as_ref(), alive, active_runs));

    let launchd_relevant = state.as_ref().is_some_and(|s| s.mode == WatchMode::Launchd)
        || launchd::plist_path().exists();
    if cfg!(target_os = "macos") && launchd_relevant {
        match launchd::print_job() {
            Ok(raw) => {
                println!("  launchd ({}):", launchd::LABEL);
                // First occurrence only: nested subsystem blocks repeat keys
                // like `state =` with less interesting values.
                let mut seen: Vec<&str> = Vec::new();
                for line in raw.lines().map(str::trim) {
                    let Some(key) = ["state = ", "pid = ", "runs = ", "last exit code"]
                        .into_iter()
                        .find(|k| line.starts_with(k))
                    else {
                        continue;
                    };
                    if !seen.contains(&key) {
                        seen.push(key);
                        println!("    {line}");
                    }
                }
            }
            Err(_) => println!("  launchd: job {} not loaded", launchd::LABEL),
        }
    }
    Ok(())
}

/// `meguri daemon logs [-f]`: tail the daemon log recorded in state.json
/// (falling back to the detached default when no state exists).
pub fn cmd_logs(follow: bool) -> Result<()> {
    let home = config::meguri_home();
    let log_path = read_state(&home)?
        .and_then(|s| s.log_path)
        .unwrap_or_else(|| logs_dir(&home).join("watch.log"));
    if !log_path.exists() {
        bail!(
            "no daemon log at {} — start with `meguri daemon start` (or `meguri daemon install --mode launchd`)",
            log_path.display()
        );
    }
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("tail");
    cmd.arg("-n").arg("100");
    if follow {
        cmd.arg("-f");
    }
    cmd.arg(&log_path);
    let err = cmd.exec();
    bail!("exec tail failed: {err}");
}
