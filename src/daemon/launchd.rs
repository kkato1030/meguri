//! macOS launchd supervision: user LaunchAgent generation (a pure function,
//! so tests can snapshot it) and the `launchctl` lifecycle around it.
//! Non-macOS platforms get an explicit error — never a silent fallback.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, RestartPolicy};

pub const LABEL: &str = "dev.meguri.watch";

pub fn plist_path() -> PathBuf {
    dirs::home_dir()
        .expect("cannot resolve home directory")
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Reject anything but launchd-on-macOS, with an explicit reason.
pub fn validate_mode(mode: &str) -> Result<()> {
    match mode {
        "launchd" => {
            if cfg!(target_os = "macos") {
                Ok(())
            } else {
                bail!(
                    "--mode launchd requires macOS; there is no supervisor for this platform yet \
                     (systemd user units are a follow-up issue)"
                )
            }
        }
        other => bail!("unsupported daemon mode {other:?} (supported: launchd)"),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the LaunchAgent plist. Pure: everything environment-dependent
/// (exe path, PATH to bake in, log location) comes in as arguments.
pub fn render_plist(
    exe: &Path,
    policy: RestartPolicy,
    throttle_secs: u64,
    log_path: &Path,
    env: &[(String, String)],
) -> String {
    let keep_alive = match policy {
        RestartPolicy::Never => String::new(),
        RestartPolicy::OnFailure => "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n"
            .to_string(),
        RestartPolicy::Always => "\t<key>KeepAlive</key>\n\t<true/>\n".to_string(),
    };
    let env_xml: String = env
        .iter()
        .map(|(k, v)| {
            format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            )
        })
        .collect();
    let exe = xml_escape(&exe.display().to_string());
    let log = xml_escape(&log_path.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>watch</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
{keep_alive}	<key>ThrottleInterval</key>
	<integer>{throttle_secs}</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>EnvironmentVariables</key>
	<dict>
{env_xml}	</dict>
</dict>
</plist>
"#
    )
}

/// Environment to bake into the plist: launchd's default PATH misses the
/// homebrew `gh`/`tmux`/`herdr`/`claude`, so the installing user's PATH is
/// captured verbatim, along with meguri-relevant overrides if set.
pub fn launch_env() -> Vec<(String, String)> {
    let mut env = vec![(super::SUPERVISED_ENV.to_string(), "launchd".to_string())];
    for key in ["PATH", "HERDR_SOCKET_PATH", "MEGURI_HOME"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    env
}

/// `meguri daemon install --mode <mode>`: render the plist and bootstrap it.
/// Config changes (policy/throttle) apply by re-running install.
pub fn cmd_install(mode: &str) -> Result<()> {
    validate_mode(mode)?;
    let cfg = Config::load()?;
    if cfg.projects.is_empty() {
        bail!(
            "no projects configured — edit {}",
            config::config_path().display()
        );
    }
    let exe = std::env::current_exe().context("cannot resolve the meguri executable path")?;
    let home = config::meguri_home();
    let log_path = super::logs_dir(&home).join("launchd.log");
    std::fs::create_dir_all(super::logs_dir(&home))?;

    let plist = render_plist(
        &exe,
        cfg.daemon.restart_policy,
        cfg.daemon.throttle_secs,
        &log_path,
        &launch_env(),
    );
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Re-install over a loaded job: bootout first so bootstrap re-reads the plist.
    let _ = bootout();
    std::fs::write(&path, plist).with_context(|| format!("cannot write {}", path.display()))?;
    bootstrap(&path)?;
    println!("installed LaunchAgent {LABEL}");
    println!("  plist:    {}", path.display());
    println!(
        "  policy:   {} (throttle {}s) — change in config.toml, then re-run install",
        cfg.daemon.restart_policy.as_str(),
        cfg.daemon.throttle_secs
    );
    println!("  log:      {}", log_path.display());
    println!("  status:   meguri daemon status");
    Ok(())
}

/// `meguri daemon uninstall`: bootout + delete the plist.
pub fn cmd_uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd is only supported on macOS — nothing to uninstall here");
    }
    let _ = bootout();
    let path = plist_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("removed {}", path.display());
    } else {
        println!("no LaunchAgent installed ({} absent)", path.display());
    }
    Ok(())
}

fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

pub fn bootstrap(plist: &Path) -> Result<()> {
    run_launchctl(&[
        "bootstrap",
        &gui_domain(),
        plist.to_str().context("plist path is not UTF-8")?,
    ])
}

pub fn bootout() -> Result<()> {
    run_launchctl(&["bootout", &format!("{}/{LABEL}", gui_domain())])
}

/// Kill-and-restart the job in place (used by `daemon restart`).
pub fn kickstart() -> Result<()> {
    run_launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", gui_domain())])
}

/// The supervisor's own view (`launchctl print`): state, pid, restart runs,
/// last exit code — surfaced by `daemon status`.
pub fn print_job() -> Result<String> {
    let out = std::process::Command::new("launchctl")
        .args(["print", &format!("{}/{LABEL}", gui_domain())])
        .output()
        .context("cannot run launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl print failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .context("cannot run launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
