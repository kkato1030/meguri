//! Locate the agent's native session id for a worktree, so a reclaimed pane
//! stays resumable (`claude --resume <id>`, #6/#13).
//!
//! Claude Code keeps one transcript per session under
//! `<config dir>/projects/<munged cwd>/<session-id>.jsonl`, where the munged
//! name replaces every non-alphanumeric character of the absolute cwd with
//! `-`. The newest transcript for the worktree is the pane's session.

use std::path::{Path, PathBuf};

use crate::config::AgentProfile;

/// Where the agent keeps its session transcripts: the configured override,
/// else `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn session_root(agent: &AgentProfile) -> PathBuf {
    if let Some(dir) = &agent.session_dir {
        return dir.clone();
    }
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

/// Claude Code's directory name for a project cwd.
fn project_dir_name(worktree: &Path) -> String {
    worktree
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The newest session id recorded for `worktree`, or None when the agent
/// never ran there (best-effort: reclamation proceeds either way).
pub fn latest_session_id(session_root: &Path, worktree: &Path) -> Option<String> {
    let dir = session_root
        .join("projects")
        .join(project_dir_name(worktree));
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, id.to_string()));
        }
    }
    newest.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_name_munges_non_alphanumerics() {
        assert_eq!(
            project_dir_name(Path::new("/Users/ka.kato/wt_1/proj")),
            "-Users-ka-kato-wt-1-proj"
        );
    }

    #[test]
    fn latest_session_id_picks_newest_transcript() {
        let root = tempfile::tempdir().unwrap();
        let worktree = Path::new("/wt/demo/branch");
        assert_eq!(latest_session_id(root.path(), worktree), None);

        let dir = root.path().join("projects").join("-wt-demo-branch");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("session-old.jsonl");
        let new = dir.join("session-new.jsonl");
        std::fs::write(&old, "{}").unwrap();
        std::fs::write(&new, "{}").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        std::fs::write(dir.join("not-a-transcript.txt"), "").unwrap();

        assert_eq!(
            latest_session_id(root.path(), worktree).as_deref(),
            Some("session-new")
        );
    }

    #[test]
    fn session_root_prefers_configured_override() {
        let agent = AgentProfile {
            session_dir: Some(PathBuf::from("/custom/claude")),
            ..AgentProfile::default()
        };
        assert_eq!(session_root(&agent), PathBuf::from("/custom/claude"));
    }
}
