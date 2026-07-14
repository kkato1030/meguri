//! Embedded agent skill/rule sources and their install targets.
//!
//! `skills/meguri/` (issue #147) is embedded into the binary via
//! `include_str!`, so "binary version" and "skill version" are the same
//! thing — updating the skill rides the normal release process instead of a
//! separate distribution channel (ADR 0009). `install`/`status` only
//! implement the `claude` target for now; `Target` is the extension point
//! other agent CLIs plug into later (issue #150).

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// One file under `skills/meguri/`, embedded at build time.
pub struct SkillFile {
    /// Path relative to the skill root, e.g. `"SKILL.md"` or
    /// `"references/setup.md"`.
    pub rel_path: &'static str,
    pub content: &'static str,
}

pub const SKILL_FILES: &[SkillFile] = &[
    SkillFile {
        rel_path: "SKILL.md",
        content: include_str!("../skills/meguri/SKILL.md"),
    },
    SkillFile {
        rel_path: "references/operate.md",
        content: include_str!("../skills/meguri/references/operate.md"),
    },
    SkillFile {
        rel_path: "references/repo-rule-fragment.md",
        content: include_str!("../skills/meguri/references/repo-rule-fragment.md"),
    },
    SkillFile {
        rel_path: "references/setup.md",
        content: include_str!("../skills/meguri/references/setup.md"),
    },
];

/// Markers wrapping the rule fragment wherever it's installed, so a re-run
/// can find and replace exactly what it wrote last time instead of
/// duplicating itself (idempotent `--project` install).
const MARKER_BEGIN: &str = "<!-- meguri:agent-skills:begin (managed by `meguri agent-skills install --project`; do not edit between markers) -->";
const MARKER_END: &str = "<!-- meguri:agent-skills:end -->";

/// `references/repo-rule-fragment.md` also carries a human-facing preamble
/// (for repos folding it in by hand); these delimit the actual payload we
/// install, so the preamble doesn't end up inside `.claude/rules/meguri.md`.
const SOURCE_FRAGMENT_START: &str = "<!-- meguri:rule-fragment:start -->";
const SOURCE_FRAGMENT_END: &str = "<!-- meguri:rule-fragment:end -->";

fn rule_fragment_block() -> String {
    let source = SKILL_FILES
        .iter()
        .find(|f| f.rel_path == "references/repo-rule-fragment.md")
        .expect("repo-rule-fragment.md is always embedded")
        .content;
    let start = source
        .find(SOURCE_FRAGMENT_START)
        .map(|i| i + SOURCE_FRAGMENT_START.len())
        .expect("repo-rule-fragment.md has a start marker");
    let end = source
        .find(SOURCE_FRAGMENT_END)
        .expect("repo-rule-fragment.md has an end marker");
    let body = source[start..end].trim();
    format!("{MARKER_BEGIN}\n{body}\n{MARKER_END}\n")
}

/// Which agent CLI's on-disk conventions to target. `--target` exists to
/// keep this mapping out of the core install/status logic below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Claude,
}

impl Target {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(Target::Claude),
            other => bail!("unknown --target {other:?} (supported targets: claude)"),
        }
    }

    /// User-level skill directory for this target, e.g.
    /// `~/.claude/skills/meguri/`.
    fn user_skill_dir(self, home: &Path) -> PathBuf {
        match self {
            Target::Claude => home.join(".claude").join("skills").join("meguri"),
        }
    }

    /// Project-level rule fragment file for this target. Claude Code reads
    /// `.claude/rules/*.md` directly (see README's apm section), so that's
    /// where the fragment goes rather than into `AGENTS.md`/`CLAUDE.md`.
    fn project_rule_path(self, repo_root: &Path) -> PathBuf {
        match self {
            Target::Claude => repo_root.join(".claude").join("rules").join("meguri.md"),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Claude => write!(f, "claude"),
        }
    }
}

/// Resolve the real user home directory (not `MEGURI_HOME` — that's
/// meguri's own state dir, unrelated to where an agent CLI keeps skills).
pub fn resolve_home() -> Result<PathBuf> {
    dirs::home_dir().context("cannot resolve home directory (no $HOME)")
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FileOutcome {
    Created,
    Updated,
    Unchanged,
    /// Existing content differs from the embedded source but wasn't
    /// touched (no `--force`) — don't silently clobber hand edits.
    Blocked,
}

pub struct FileReport {
    pub path: PathBuf,
    pub outcome: FileOutcome,
    /// Set whenever there was prior content that differed from what we
    /// wrote (or would have written).
    pub diff: Option<String>,
}

pub struct InstallReport {
    pub target: Target,
    pub files: Vec<FileReport>,
}

impl InstallReport {
    pub fn has_blocked(&self) -> bool {
        self.files.iter().any(|f| f.outcome == FileOutcome::Blocked)
    }
}

/// Write `content` to `path`. If `path` already exists with different
/// content, only overwrite when `force` — otherwise report `Blocked` with a
/// diff so the caller can show it without touching the file.
fn write_managed(path: &Path, content: &str, force: bool) -> Result<FileReport> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::read_to_string(path) {
        Err(_) => {
            std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
            Ok(FileReport {
                path: path.to_path_buf(),
                outcome: FileOutcome::Created,
                diff: None,
            })
        }
        Ok(prev) if prev == content => Ok(FileReport {
            path: path.to_path_buf(),
            outcome: FileOutcome::Unchanged,
            diff: None,
        }),
        Ok(prev) => {
            let diff = line_diff(&prev, content);
            if force {
                std::fs::write(path, content)
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(FileReport {
                    path: path.to_path_buf(),
                    outcome: FileOutcome::Updated,
                    diff: Some(diff),
                })
            } else {
                Ok(FileReport {
                    path: path.to_path_buf(),
                    outcome: FileOutcome::Blocked,
                    diff: Some(diff),
                })
            }
        }
    }
}

/// Minimal unified-style line diff — enough to show *that*, and roughly
/// where, two small text files differ. Not a full Myers diff; these files
/// are short enough that an O(n*m) LCS table is plenty fast.
fn line_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let n = old_lines.len();
    let m = new_lines.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old_lines[i] == new_lines[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push('-');
            out.push_str(old_lines[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push('+');
            out.push_str(new_lines[j]);
            out.push('\n');
            j += 1;
        }
    }
    while i < n {
        out.push('-');
        out.push_str(old_lines[i]);
        out.push('\n');
        i += 1;
    }
    while j < m {
        out.push('+');
        out.push_str(new_lines[j]);
        out.push('\n');
        j += 1;
    }
    out
}

/// Install the user-level skill (`SKILL.md` + `references/`) under `home`.
pub fn install_user_skill(target: Target, home: &Path, force: bool) -> Result<InstallReport> {
    let dir = target.user_skill_dir(home);
    let files = SKILL_FILES
        .iter()
        .map(|f| write_managed(&dir.join(f.rel_path), f.content, force))
        .collect::<Result<Vec<_>>>()?;
    Ok(InstallReport { target, files })
}

/// Install the project-level rule fragment into `repo_root`.
pub fn install_project_fragment(
    target: Target,
    repo_root: &Path,
    force: bool,
) -> Result<InstallReport> {
    let path = target.project_rule_path(repo_root);
    let report = write_managed_fragment(&path, &rule_fragment_block(), force)?;
    Ok(InstallReport {
        target,
        files: vec![report],
    })
}

/// If `existing` contains our exact `MARKER_BEGIN..MARKER_END` span, return
/// `existing` with that span replaced by `block` (which itself starts with
/// `MARKER_BEGIN` and ends with `MARKER_END\n`) — everything outside the
/// span is left untouched. `None` if the markers aren't both present in
/// order, meaning there's nothing recognizable to merge into.
fn upsert_marked_span(existing: &str, block: &str) -> Option<String> {
    let start = existing.find(MARKER_BEGIN)?;
    let end_marker_at = existing.find(MARKER_END)?;
    if end_marker_at < start {
        return None;
    }
    let mut end = end_marker_at + MARKER_END.len();
    // `block` already supplies the newline that terminates its own
    // MARKER_END line, so fold the existing one into the replaced span too
    // — otherwise a well-formed prior file (nothing after the marker but
    // its own trailing newline) would gain a duplicate blank line.
    if existing[end..].starts_with('\n') {
        end += 1;
    }
    Some(format!("{}{block}{}", &existing[..start], &existing[end..]))
}

/// Write the project-level rule fragment. A previous install's marker span
/// is always safe to replace on a normal (re-)install — it's our own
/// managed content, and a version bump changing its body isn't a hand edit.
/// Only content outside the markers (or a file with no recognizable markers
/// at all) is treated as user-owned and gated behind `--force`.
fn write_managed_fragment(path: &Path, block: &str, force: bool) -> Result<FileReport> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let Some(prev) = std::fs::read_to_string(path).ok() else {
        std::fs::write(path, block).with_context(|| format!("writing {}", path.display()))?;
        return Ok(FileReport {
            path: path.to_path_buf(),
            outcome: FileOutcome::Created,
            diff: None,
        });
    };
    if prev == block {
        return Ok(FileReport {
            path: path.to_path_buf(),
            outcome: FileOutcome::Unchanged,
            diff: None,
        });
    }
    match upsert_marked_span(&prev, block) {
        Some(new_full) if new_full == prev => Ok(FileReport {
            path: path.to_path_buf(),
            outcome: FileOutcome::Unchanged,
            diff: None,
        }),
        Some(new_full) => {
            let diff = line_diff(&prev, &new_full);
            std::fs::write(path, &new_full)
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(FileReport {
                path: path.to_path_buf(),
                outcome: FileOutcome::Updated,
                diff: Some(diff),
            })
        }
        None => {
            let diff = line_diff(&prev, block);
            if force {
                std::fs::write(path, block)
                    .with_context(|| format!("writing {}", path.display()))?;
                Ok(FileReport {
                    path: path.to_path_buf(),
                    outcome: FileOutcome::Updated,
                    diff: Some(diff),
                })
            } else {
                Ok(FileReport {
                    path: path.to_path_buf(),
                    outcome: FileOutcome::Blocked,
                    diff: Some(diff),
                })
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StatusState {
    Missing,
    UpToDate,
    /// Installed, but differs from what this binary would install — either
    /// hand-edited or installed by an older/newer meguri build.
    Drifted,
}

pub struct StatusEntry {
    pub path: PathBuf,
    pub state: StatusState,
}

fn status_of(path: &Path, expected: &str) -> StatusEntry {
    let state = match std::fs::read_to_string(path) {
        Err(_) => StatusState::Missing,
        Ok(actual) if actual == expected => StatusState::UpToDate,
        Ok(_) => StatusState::Drifted,
    };
    StatusEntry {
        path: path.to_path_buf(),
        state,
    }
}

pub fn status_user_skill(target: Target, home: &Path) -> Vec<StatusEntry> {
    let dir = target.user_skill_dir(home);
    SKILL_FILES
        .iter()
        .map(|f| status_of(&dir.join(f.rel_path), f.content))
        .collect()
}

pub fn status_project_fragment(target: Target, repo_root: &Path) -> StatusEntry {
    status_of(&target.project_rule_path(repo_root), &rule_fragment_block())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_target_only() {
        assert_eq!(Target::parse("claude").unwrap(), Target::Claude);
        assert!(Target::parse("codex").is_err());
    }

    #[test]
    fn install_user_skill_creates_all_files_fresh() {
        let home = tempfile::tempdir().unwrap();
        let report = install_user_skill(Target::Claude, home.path(), false).unwrap();
        assert_eq!(report.files.len(), SKILL_FILES.len());
        assert!(
            report
                .files
                .iter()
                .all(|f| f.outcome == FileOutcome::Created)
        );
        assert!(!report.has_blocked());
        let installed = home.path().join(".claude/skills/meguri/SKILL.md");
        assert_eq!(
            std::fs::read_to_string(installed).unwrap(),
            SKILL_FILES[0].content
        );
    }

    #[test]
    fn install_user_skill_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        install_user_skill(Target::Claude, home.path(), false).unwrap();
        let second = install_user_skill(Target::Claude, home.path(), false).unwrap();
        assert!(
            second
                .files
                .iter()
                .all(|f| f.outcome == FileOutcome::Unchanged)
        );
    }

    #[test]
    fn install_user_skill_does_not_clobber_hand_edits_without_force() {
        let home = tempfile::tempdir().unwrap();
        install_user_skill(Target::Claude, home.path(), false).unwrap();
        let skill_md = home.path().join(".claude/skills/meguri/SKILL.md");
        std::fs::write(&skill_md, "hand-edited content").unwrap();

        let report = install_user_skill(Target::Claude, home.path(), false).unwrap();
        let entry = report
            .files
            .iter()
            .find(|f| f.path == skill_md)
            .expect("SKILL.md entry present");
        assert_eq!(entry.outcome, FileOutcome::Blocked);
        assert!(entry.diff.as_ref().unwrap().contains("-hand-edited"));
        assert_eq!(
            std::fs::read_to_string(&skill_md).unwrap(),
            "hand-edited content"
        );

        let forced = install_user_skill(Target::Claude, home.path(), true).unwrap();
        let entry = forced
            .files
            .iter()
            .find(|f| f.path == skill_md)
            .expect("SKILL.md entry present");
        assert_eq!(entry.outcome, FileOutcome::Updated);
        assert_eq!(
            std::fs::read_to_string(&skill_md).unwrap(),
            SKILL_FILES[0].content
        );
    }

    #[test]
    fn install_project_fragment_wraps_in_markers_and_is_idempotent() {
        let repo = tempfile::tempdir().unwrap();
        let report = install_project_fragment(Target::Claude, repo.path(), false).unwrap();
        assert_eq!(report.files[0].outcome, FileOutcome::Created);
        let path = repo.path().join(".claude/rules/meguri.md");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(MARKER_BEGIN));
        assert!(written.trim_end().ends_with(MARKER_END));

        let second = install_project_fragment(Target::Claude, repo.path(), false).unwrap();
        assert_eq!(second.files[0].outcome, FileOutcome::Unchanged);
    }

    #[test]
    fn install_project_fragment_reinstalls_over_a_prior_managed_block_without_force() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(".claude/rules/meguri.md");
        // Simulate a file written by an older meguri: same markers, older body.
        let stale =
            format!("{MARKER_BEGIN}\nold body from a previous meguri version\n{MARKER_END}\n");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &stale).unwrap();

        let report = install_project_fragment(Target::Claude, repo.path(), false).unwrap();
        assert_eq!(report.files[0].outcome, FileOutcome::Updated);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, rule_fragment_block());
    }

    #[test]
    fn install_project_fragment_preserves_content_outside_the_markers() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(".claude/rules/meguri.md");
        let stale = format!(
            "# repo-specific notes\n\nkeep this.\n\n{MARKER_BEGIN}\nold body\n{MARKER_END}\n\nkeep this too.\n"
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &stale).unwrap();

        let report = install_project_fragment(Target::Claude, repo.path(), false).unwrap();
        assert_eq!(report.files[0].outcome, FileOutcome::Updated);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# repo-specific notes\n\nkeep this.\n\n"));
        assert!(written.ends_with("\n\nkeep this too.\n"));
        assert!(written.contains(&rule_fragment_block()));
    }

    #[test]
    fn install_project_fragment_without_markers_needs_force() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join(".claude/rules/meguri.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "hand-written, no markers at all").unwrap();

        let blocked = install_project_fragment(Target::Claude, repo.path(), false).unwrap();
        assert_eq!(blocked.files[0].outcome, FileOutcome::Blocked);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hand-written, no markers at all"
        );

        let forced = install_project_fragment(Target::Claude, repo.path(), true).unwrap();
        assert_eq!(forced.files[0].outcome, FileOutcome::Updated);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rule_fragment_block()
        );
    }

    #[test]
    fn status_reports_missing_up_to_date_and_drifted() {
        let home = tempfile::tempdir().unwrap();
        let before = status_user_skill(Target::Claude, home.path());
        assert!(before.iter().all(|e| e.state == StatusState::Missing));

        install_user_skill(Target::Claude, home.path(), false).unwrap();
        let after = status_user_skill(Target::Claude, home.path());
        assert!(after.iter().all(|e| e.state == StatusState::UpToDate));

        let skill_md = home.path().join(".claude/skills/meguri/SKILL.md");
        std::fs::write(&skill_md, "drift").unwrap();
        let drifted = status_user_skill(Target::Claude, home.path());
        let entry = drifted.iter().find(|e| e.path == skill_md).unwrap();
        assert_eq!(entry.state, StatusState::Drifted);
    }

    #[test]
    fn status_project_fragment_round_trips() {
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(
            status_project_fragment(Target::Claude, repo.path()).state,
            StatusState::Missing
        );
        install_project_fragment(Target::Claude, repo.path(), false).unwrap();
        assert_eq!(
            status_project_fragment(Target::Claude, repo.path()).state,
            StatusState::UpToDate
        );
    }
}
