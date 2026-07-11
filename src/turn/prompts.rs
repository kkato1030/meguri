//! Prompt files, the trigger line, and the result-file contract.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const MEGURI_DIR: &str = ".meguri";
pub const RESULT_FILE: &str = "result.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Success,
    Failure,
    NeedsHuman,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnResultFile {
    pub turn_id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub summary: String,
    /// Agent-authored pull-request description (Markdown), used as the PR
    /// body when present; `summary` is the fallback.
    #[serde(default)]
    pub pr_body: Option<String>,
}

pub fn meguri_dir(worktree: &Path) -> PathBuf {
    worktree.join(MEGURI_DIR)
}

pub fn result_path(worktree: &Path) -> PathBuf {
    meguri_dir(worktree).join(RESULT_FILE)
}

pub fn prompt_path(worktree: &Path, turn_id: &str) -> PathBuf {
    meguri_dir(worktree).join(format!("prompt-{turn_id}.md"))
}

/// The single line typed into the agent's TUI to start a turn.
pub fn trigger_line(turn_id: &str) -> String {
    format!("Read the file {MEGURI_DIR}/prompt-{turn_id}.md and carry it out completely.")
}

/// Contract block appended to every prompt body.
fn completion_contract(turn_id: &str) -> String {
    format!(
        r#"---

## Completion contract (mandatory)

When you have FULLY completed the task above, write a JSON file at
`{MEGURI_DIR}/{RESULT_FILE}` (relative to the repository root) containing exactly:

    {{"turn_id": "{turn_id}", "status": "success", "summary": "<one concise paragraph of what you did>", "pr_body": "<Markdown pull-request description>"}}

- `status` must be one of: "success" (task done), "failure" (you tried and
  cannot complete it), "needs_human" (a human decision or missing information
  blocks you — explain what you need in the summary).
- `pr_body` (on success): a Markdown pull-request description of what you
  actually changed. If the prompt includes a PR template, fill in each of its
  sections; escape newlines as \n inside the JSON string.
- WRITE THE FILE; do not merely print the JSON to the terminal.
- Do not commit or stage anything under `{MEGURI_DIR}/`.
- If you are unsure whether you are done, prefer "needs_human" over guessing."#
    )
}

/// Write the full prompt file for a turn; returns its path.
pub fn write_prompt_file(worktree: &Path, turn_id: &str, body: &str) -> Result<PathBuf> {
    let dir = meguri_dir(worktree);
    std::fs::create_dir_all(&dir)?;
    let path = prompt_path(worktree, turn_id);
    let content = format!(
        "<!-- meguri prompt -->\nturn_id: {turn_id}\n\n{body}\n\n{contract}\n",
        contract = completion_contract(turn_id)
    );
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Remove any stale result file before a new turn starts.
pub fn clear_result(worktree: &Path) -> Result<()> {
    let path = result_path(worktree);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Read the result file if it exists and belongs to `turn_id`.
/// A file for another turn (stale) or unparseable content yields None.
pub fn read_result(worktree: &Path, turn_id: &str) -> Option<TurnResultFile> {
    let raw = std::fs::read_to_string(result_path(worktree)).ok()?;
    let parsed: TurnResultFile = serde_json::from_str(raw.trim()).ok()?;
    if parsed.turn_id == turn_id {
        Some(parsed)
    } else {
        None
    }
}

/// One-line reminder sent when the agent goes quiet without a result.
pub fn nudge_line(turn_id: &str) -> String {
    format!(
        "If you finished the task from {MEGURI_DIR}/prompt-{turn_id}.md, write {MEGURI_DIR}/{RESULT_FILE} as instructed (turn_id {turn_id}); otherwise continue working on it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_file_contains_contract_and_turn_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_prompt_file(dir.path(), "abc-123", "Implement the thing.").unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("turn_id: abc-123"));
        assert!(content.contains("Implement the thing."));
        assert!(content.contains(r#""turn_id": "abc-123""#));
        assert!(content.contains("needs_human"));
        assert!(content.contains("pr_body"));
    }

    #[test]
    fn read_result_matches_turn_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done"}"#,
        )
        .unwrap();
        assert!(read_result(dir.path(), "t1").is_some());
        assert!(read_result(dir.path(), "t2").is_none());

        std::fs::write(result_path(dir.path()), "not json").unwrap();
        assert!(read_result(dir.path(), "t1").is_none());
    }

    #[test]
    fn result_pr_body_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done"}"#,
        )
        .unwrap();
        assert_eq!(read_result(dir.path(), "t1").unwrap().pr_body, None);

        std::fs::write(
            result_path(dir.path()),
            r###"{"turn_id":"t1","status":"success","summary":"done","pr_body":"## Summary\nDid it."}"###,
        )
        .unwrap();
        assert_eq!(
            read_result(dir.path(), "t1").unwrap().pr_body.as_deref(),
            Some("## Summary\nDid it.")
        );
    }

    #[test]
    fn clear_result_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        clear_result(dir.path()).unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(result_path(dir.path()), "{}").unwrap();
        clear_result(dir.path()).unwrap();
        assert!(!result_path(dir.path()).exists());
    }
}
