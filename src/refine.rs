//! `meguri add`'s refine step: a headless, best-effort tidy-up of a raw memo
//! into a structured issue title/body.
//!
//! Refine lives OUTSIDE the issue↔pane↔session lifetime model (#92): one
//! headless call, no worktree, read-only on the repo. The orchestrator (not
//! the model) owns preserving the original memo verbatim and writing the
//! result back to the forge (ADR 0006). The [`Refiner`] trait keeps the actual
//! agent call injectable so `meguri add` can be tested without spawning a CLI.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

/// A refined issue as parsed from the refiner's stdout: a summarized title and
/// a structured body. The verbatim original memo is deliberately NOT here —
/// the caller appends it, so the model can never drop it (ADR 0006 原則2).
#[derive(Debug, Clone)]
pub struct Refined {
    pub title: String,
    pub body: String,
}

/// The refine step, abstracted so a test can inject a fixed result (or a
/// failure) without running an agent CLI.
#[async_trait]
pub trait Refiner: Send + Sync {
    /// Refine `text` into a title/body, reading `repo_path` read-only. Any
    /// `Err` means "leave the issue raw" — refine is best-effort and never
    /// fails the capture.
    async fn refine(&self, text: &str, repo_path: &Path, language: Option<&str>)
    -> Result<Refined>;
}

/// How long a single refine call may take before `meguri add` gives up and
/// leaves the issue raw. Refine is a few-second tidy-up; a hang must not turn
/// the low-friction intake into a wait.
const REFINE_TIMEOUT: Duration = Duration::from_secs(120);

/// Headless one-shot refiner: `{command} {argv} <prompt>` run in the repo,
/// capturing stdout. Read-only by construction — `argv` is the profile's
/// `headless_args`, which never carries yolo (spec 論点1/論点4). A timeout and
/// Ctrl-C both just abort refine (the issue stays raw); neither hangs the
/// command.
pub struct HeadlessRefiner {
    pub command: String,
    pub argv: Vec<String>,
}

#[async_trait]
impl Refiner for HeadlessRefiner {
    async fn refine(
        &self,
        text: &str,
        repo_path: &Path,
        language: Option<&str>,
    ) -> Result<Refined> {
        let prompt = refine_prompt(text, language);
        let child = tokio::process::Command::new(&self.command)
            .args(&self.argv)
            .arg(&prompt)
            .current_dir(repo_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning refiner `{}`", self.command))?;

        let out = tokio::select! {
            res = tokio::time::timeout(REFINE_TIMEOUT, child.wait_with_output()) => match res {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => return Err(e).context("waiting for the refiner"),
                Err(_) => bail!("refiner timed out after {}s", REFINE_TIMEOUT.as_secs()),
            },
            // Ctrl-C aborts refine; dropping the child (kill_on_drop) stops the
            // agent. Capture already succeeded, so the issue stays raw.
            _ = tokio::signal::ctrl_c() => bail!("interrupted (Ctrl-C)"),
        };

        if !out.status.success() {
            bail!(
                "refiner `{}` exited {}: {}",
                self.command,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim(),
            );
        }
        parse_refined(&String::from_utf8_lossy(&out.stdout))
    }
}

/// The refine prompt: tidy the memo into title/body, grounded in a read-only
/// read of the repo, strictly scoped, output as a single JSON object. The
/// original memo is explicitly kept out of the model's output — meguri appends
/// it verbatim.
pub fn refine_prompt(text: &str, language: Option<&str>) -> String {
    let lang = match language {
        Some(l) => format!(" Write the title and body in {l}."),
        None => String::new(),
    };
    format!(
        "You are turning a rough one-line memo into a well-formed GitHub issue.\n\n\
         Memo:\n{text}\n\n\
         Read the repository under the current directory (read-only) to ground \
         your guesses about where this belongs, then produce a concise, \
         specific title and a structured body. Pick a skeleton that fits the \
         memo's kind — e.g. for a bug: 症状 / 期待動作 / 関連しそうな箇所; \
         for a task: 背景 / やること / 関連しそうな箇所.\n\n\
         Hard rules:\n\
         - Do NOT widen the scope beyond the memo. No label guesses, no \
         priority, no duplicate detection.\n\
         - Do NOT write or edit any file, and do NOT commit. Read only.\n\
         - Do NOT include the original memo in your output — meguri preserves \
         it verbatim itself.\n\
         - Output ONLY a single JSON object and nothing else (no prose, no \
         code fence): {{\"title\": \"...\", \"body\": \"...\"}}.{lang}"
    )
}

/// Parse the refiner's stdout into a [`Refined`]. Best-effort: tolerates a
/// code fence or surrounding whitespace by taking the outermost `{...}`, but a
/// missing/unparseable object or an empty title is an error (→ leave raw).
pub fn parse_refined(stdout: &str) -> Result<Refined> {
    #[derive(Deserialize)]
    struct Raw {
        title: String,
        #[serde(default)]
        body: String,
    }
    let json = extract_json_object(stdout).context("refiner produced no JSON object")?;
    let raw: Raw = serde_json::from_str(json).context("parsing the refiner's JSON")?;
    let title = raw.title.trim().to_string();
    if title.is_empty() {
        bail!("refiner returned an empty title");
    }
    Ok(Refined {
        title,
        body: raw.body.trim().to_string(),
    })
}

/// The outermost `{...}` span of a string, so a JSON object survives a stray
/// code fence or trailing newline the model may add despite instructions.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| &s[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refined_accepts_bare_and_fenced_json() {
        let bare = parse_refined(r#"{"title":"T","body":"B"}"#).unwrap();
        assert_eq!(bare.title, "T");
        assert_eq!(bare.body, "B");

        let fenced = parse_refined("```json\n{\"title\": \"T2\", \"body\": \"B2\"}\n```").unwrap();
        assert_eq!(fenced.title, "T2");
        assert_eq!(fenced.body, "B2");
    }

    #[test]
    fn parse_refined_rejects_junk_and_empty_title() {
        assert!(parse_refined("no json here").is_err());
        assert!(parse_refined(r#"{"title":"  ","body":"B"}"#).is_err());
    }

    #[test]
    fn refine_prompt_pins_language_and_forbids_writes() {
        let p = refine_prompt("login redirect is weird", Some("日本語"));
        assert!(p.contains("login redirect is weird"));
        assert!(p.contains("日本語"));
        assert!(p.contains("Read only"));
        assert!(p.contains("preserves"));
    }
}
