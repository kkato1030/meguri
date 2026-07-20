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
    /// The agent found that a design decision must precede implementation
    /// (issue #22). Only the worker's execute prompt invites this status;
    /// everywhere else it escalates like `NeedsHuman`.
    NeedsPlan,
    /// The agent found the issue too big for one spec and proposes to split
    /// it into sub-issues (issue #24), described in
    /// [`TurnResultFile::children`]. Only the planner's execute prompt
    /// invites this status; everywhere else it escalates like `NeedsHuman`.
    Decompose,
}

/// One sub-issue proposed by a `decompose` turn (issue #24). meguri files
/// the issue itself — the agent only describes it.
#[derive(Debug, Clone, Deserialize)]
pub struct ChildIssue {
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// How the child enters the loops: "ready" (small enough to implement
    /// directly), "plan" (needs its own design pass first), or "human" (a task
    /// meguri cannot run — filed with no trigger label so discovery never
    /// picks it up and a human closes it; issue #154).
    pub kind: String,
    /// Zero-based indices of *earlier* `children` entries this one depends
    /// on; meguri wires them as GitHub-native `blocked_by`.
    #[serde(default)]
    pub blocked_by: Vec<usize>,
    /// Which project (repository) to file this child in. `None` (the default)
    /// files it in the parent issue's own repo — the existing behavior. A
    /// value must name a workspace sibling of the parent's project; the
    /// planner rejects anything else, keeping issue-filing scope pinned to the
    /// host operator's config (issue #154 / ADR 0009).
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnResultFile {
    pub turn_id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub summary: String,
    /// Agent-authored PR/commit subject (issue #136): a short, imperative
    /// line describing the actual change, used as the PR title instead of
    /// the issue title when present. `summary`/`pr_body` follow the same
    /// optional-field shape.
    #[serde(default)]
    pub subject: Option<String>,
    /// Agent-authored pull-request description (Markdown), used as the PR
    /// body when present; `summary` is the fallback.
    #[serde(default)]
    pub pr_body: Option<String>,
    /// The agent CLI's own session id (e.g. a Claude Code session UUID),
    /// letting recovery `--resume` the conversation after the pane dies.
    #[serde(default)]
    pub agent_session_id: Option<String>,
    /// Sub-issues proposed by a `decompose` turn, in dependency order
    /// (issue #24). Empty for every other status.
    #[serde(default)]
    pub children: Vec<ChildIssue>,
}

pub fn meguri_dir(worktree: &Path) -> PathBuf {
    worktree.join(MEGURI_DIR)
}

pub fn result_path(worktree: &Path) -> PathBuf {
    meguri_dir(worktree).join(RESULT_FILE)
}

/// The per-turn result file for an isolated turn (issue #214):
/// `.meguri/result-<turn_id>.json`. Parallel round-1 review turns each write
/// their own so they never race on the single `result.json`.
pub fn isolated_result_path(worktree: &Path, turn_id: &str) -> PathBuf {
    meguri_dir(worktree).join(format!("result-{turn_id}.json"))
}

pub fn prompt_path(worktree: &Path, turn_id: &str) -> PathBuf {
    meguri_dir(worktree).join(format!("prompt-{turn_id}.md"))
}

/// The single line typed into the agent's TUI to start a turn.
pub fn trigger_line(turn_id: &str) -> String {
    format!("Read the file {MEGURI_DIR}/prompt-{turn_id}.md and carry it out completely.")
}

/// Contract block appended to every prompt body. `isolated` (issue #214) swaps
/// the shared `result.json` for a per-turn `result-<turn_id>.json`, so parallel
/// review turns never race on one file; `false` yields the historical wording
/// byte-for-byte.
fn completion_contract(turn_id: &str, isolated: bool) -> String {
    let result_file = result_file_name(turn_id, isolated);
    format!(
        r#"---

## Completion contract (mandatory)

When you have FULLY completed the task above, write a JSON file at
`{MEGURI_DIR}/{result_file}` (relative to the repository root) containing exactly:

    {{"turn_id": "{turn_id}", "status": "success", "subject": "<imperative one-line description of the actual change>", "summary": "<one concise paragraph of what you did>", "pr_body": "<Markdown pull-request description>"}}

- `status` must be one of: "success" (task done), "failure" (you tried and
  cannot complete it), "needs_human" (a human decision or missing information
  blocks you — explain what you need in the summary).
- `subject` (optional): a short, imperative-mood line (roughly 50-72
  characters) naming what this PR actually changes — not a restatement of
  the issue's goal. It becomes the commit subject / PR title as "<subject>
  (#N)"; do not add the "(#N)" yourself, the engine appends it. If you only
  wrote a spec, say so honestly (e.g. "Add a spec for ..."); if the real
  change diverged from the issue's ask, describe what landed. Match the
  repository's existing language convention. Omit this field to fall back
  to the issue title (previous behavior).
- `pr_body` (on success): a Markdown pull-request description of what you
  actually changed. If the prompt includes a PR template, fill in each of its
  sections; escape newlines as \n inside the JSON string.
- `agent_session_id` (optional): if you know your own CLI session id (e.g.
  your Claude Code session UUID), include it so this conversation can be
  resumed if the terminal dies. Omit the field entirely if you are not sure —
  never invent one.
- WRITE THE FILE; do not merely print the JSON to the terminal.
- Do not commit or stage anything under `{MEGURI_DIR}/`.
- If you are unsure whether you are done, prefer "needs_human" over guessing."#
    )
}

/// Write the full prompt file for a turn; returns its path.
///
/// `preamble` is the project's standing-discipline block (issue #149): when
/// non-empty it is placed right after the header and before `{body}`, so it
/// reads as a preface while the completion contract stays last and
/// authoritative. An empty `preamble` yields byte-for-byte the historical
/// output.
pub fn write_prompt_file(
    worktree: &Path,
    turn_id: &str,
    body: &str,
    preamble: &str,
    isolated: bool,
) -> Result<PathBuf> {
    let dir = meguri_dir(worktree);
    std::fs::create_dir_all(&dir)?;
    let path = prompt_path(worktree, turn_id);
    let preamble_block = if preamble.is_empty() {
        String::new()
    } else {
        format!("{preamble}\n\n")
    };
    let content = format!(
        "<!-- meguri prompt -->\nturn_id: {turn_id}\n\n{preamble_block}{body}\n\n{contract}\n",
        contract = completion_contract(turn_id, isolated)
    );
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Remove any stale shared result file before a new turn starts.
pub fn clear_result(worktree: &Path) -> Result<()> {
    remove_if_present(&result_path(worktree))
}

/// Remove any stale per-turn result file before an isolated turn starts
/// (issue #214).
pub fn clear_isolated_result(worktree: &Path, turn_id: &str) -> Result<()> {
    remove_if_present(&isolated_result_path(worktree, turn_id))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Read the result file if it exists and belongs to `turn_id`. Tries the
/// per-turn `result-<turn_id>.json` first (issue #214 isolated turns), then the
/// shared `result.json`; the per-turn name embeds the id, so the two never
/// collide and the shared path stays byte-for-byte for ordinary turns. A file
/// for another turn (stale) or unparseable content yields None.
pub fn read_result(worktree: &Path, turn_id: &str) -> Option<TurnResultFile> {
    read_result_at(&isolated_result_path(worktree, turn_id), turn_id)
        .or_else(|| read_result_at(&result_path(worktree), turn_id))
}

fn read_result_at(path: &Path, turn_id: &str) -> Option<TurnResultFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: TurnResultFile = serde_json::from_str(raw.trim()).ok()?;
    if parsed.turn_id == turn_id {
        Some(parsed)
    } else {
        None
    }
}

/// The result file an agent is told to write for a turn: the shared
/// `result.json`, or (issue #214 isolated turns) the per-turn
/// `result-<turn_id>.json`. The completion contract and the stagnation nudge
/// must name the same file, or a nudged isolated reviewer would fall back to the
/// shared `result.json` and race its siblings.
fn result_file_name(turn_id: &str, isolated: bool) -> String {
    if isolated {
        format!("result-{turn_id}.json")
    } else {
        RESULT_FILE.to_string()
    }
}

/// One-line reminder sent when the agent goes quiet without a result. `isolated`
/// (issue #214) must match the value used to prepare the turn so the nudge names
/// the same result file as the completion contract did.
pub fn nudge_line(turn_id: &str, isolated: bool) -> String {
    let result_file = result_file_name(turn_id, isolated);
    format!(
        "If you finished the task from {MEGURI_DIR}/prompt-{turn_id}.md, write {MEGURI_DIR}/{result_file} as instructed (turn_id {turn_id}); otherwise continue working on it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_file_contains_contract_and_turn_id() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_prompt_file(dir.path(), "abc-123", "Implement the thing.", "", false).unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("turn_id: abc-123"));
        assert!(content.contains("Implement the thing."));
        assert!(content.contains(r#""turn_id": "abc-123""#));
        assert!(content.contains("needs_human"));
        assert!(content.contains("pr_body"));
        assert!(content.contains("subject"));
        assert!(content.contains("agent_session_id"));
    }

    #[test]
    fn empty_preamble_leaves_output_unchanged() {
        // The whole point of the empty-preamble path: a project with no
        // `[prompts]` config gets byte-for-byte the historical prompt.
        let dir = tempfile::tempdir().unwrap();
        let with = write_prompt_file(dir.path(), "t", "Body here.", "", false).unwrap();
        let content = std::fs::read_to_string(with).unwrap();
        assert_eq!(
            content,
            format!(
                "<!-- meguri prompt -->\nturn_id: t\n\nBody here.\n\n{}\n",
                completion_contract("t", false)
            )
        );
    }

    #[test]
    fn preamble_sits_before_body_and_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_prompt_file(
            dir.path(),
            "t",
            "ISSUE BODY",
            "## 恒常規律\nRead the guardrails.",
            false,
        )
        .unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        let preamble_at = content.find("恒常規律").unwrap();
        let body_at = content.find("ISSUE BODY").unwrap();
        let contract_at = content.find("Completion contract").unwrap();
        // preamble before the issue body, and the completion contract stays
        // last so it keeps final authority (ADR 0012).
        assert!(preamble_at < body_at, "preamble must precede the body");
        assert!(body_at < contract_at, "contract must stay last");
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
    fn isolated_result_read_does_not_collide_with_shared() {
        // Issue #214: a per-turn result file is read for its own turn, while a
        // stale shared result.json (a different turn) does not satisfy it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        // A stale shared result for some earlier turn.
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"execute","status":"success","summary":"prev"}"#,
        )
        .unwrap();
        // The isolated turn hasn't written yet: the shared file's mismatched id
        // must not be mistaken for this turn's result.
        assert!(read_result(dir.path(), "rev0").is_none());
        // Now the isolated turn writes its own per-turn file.
        std::fs::write(
            isolated_result_path(dir.path(), "rev0"),
            r#"{"turn_id":"rev0","status":"success","summary":"reviewed"}"#,
        )
        .unwrap();
        assert_eq!(read_result(dir.path(), "rev0").unwrap().summary, "reviewed");
        // The shared file still resolves its own (different) turn independently.
        assert_eq!(read_result(dir.path(), "execute").unwrap().summary, "prev");
    }

    #[test]
    fn isolated_contract_names_per_turn_result_file() {
        // Issue #214: the isolated contract instructs writing result-<id>.json;
        // the shared contract keeps result.json (byte-for-byte for other turns).
        assert!(completion_contract("z", true).contains(".meguri/result-z.json"));
        assert!(!completion_contract("z", true).contains(".meguri/result.json`"));
        assert!(completion_contract("z", false).contains(".meguri/result.json"));
    }

    #[test]
    fn nudge_names_same_result_file_as_contract() {
        // Issue #214: the stagnation nudge must point a stalled isolated
        // reviewer at its per-turn file, not the shared result.json its
        // siblings write; the shared-turn nudge keeps the historical wording.
        assert!(nudge_line("z", true).contains(".meguri/result-z.json"));
        assert!(!nudge_line("z", true).contains(".meguri/result.json"));
        assert!(nudge_line("z", false).contains(".meguri/result.json"));
        assert!(!nudge_line("z", false).contains("result-z.json"));
    }

    #[test]
    fn result_status_needs_plan_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"needs_plan","summary":"design first"}"#,
        )
        .unwrap();
        let result = read_result(dir.path(), "t1").unwrap();
        assert_eq!(result.status, TurnStatus::NeedsPlan);
        assert_eq!(result.summary, "design first");
    }

    #[test]
    fn result_status_decompose_parses_with_children() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"decompose","summary":"too big",
                "children":[
                  {"title":"part 1","body":"do A","kind":"ready"},
                  {"title":"part 2","kind":"plan","blocked_by":[0]}
                ]}"#,
        )
        .unwrap();
        let result = read_result(dir.path(), "t1").unwrap();
        assert_eq!(result.status, TurnStatus::Decompose);
        assert_eq!(result.children.len(), 2);
        assert_eq!(result.children[0].title, "part 1");
        assert_eq!(result.children[0].kind, "ready");
        assert!(result.children[0].blocked_by.is_empty());
        assert_eq!(result.children[1].body, "");
        assert_eq!(result.children[1].blocked_by, vec![0]);
    }

    #[test]
    fn result_children_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done"}"#,
        )
        .unwrap();
        assert!(read_result(dir.path(), "t1").unwrap().children.is_empty());
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
    fn result_subject_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done"}"#,
        )
        .unwrap();
        assert_eq!(read_result(dir.path(), "t1").unwrap().subject, None);

        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done","subject":"Cache API responses"}"#,
        )
        .unwrap();
        assert_eq!(
            read_result(dir.path(), "t1").unwrap().subject.as_deref(),
            Some("Cache API responses")
        );
    }

    #[test]
    fn result_agent_session_id_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meguri_dir(dir.path())).unwrap();
        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done"}"#,
        )
        .unwrap();
        assert_eq!(
            read_result(dir.path(), "t1").unwrap().agent_session_id,
            None
        );

        std::fs::write(
            result_path(dir.path()),
            r#"{"turn_id":"t1","status":"success","summary":"done","agent_session_id":"sess-42"}"#,
        )
        .unwrap();
        assert_eq!(
            read_result(dir.path(), "t1")
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some("sess-42")
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
