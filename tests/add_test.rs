//! `meguri add` — capture-first github-mode intake (ADR 0006 原則2: the memo
//! is stored verbatim) and the flag↔mode checks.

use std::sync::Arc;

use meguri::app::{
    AddParams, add_core, check_add_flags, github_memo, infer_project, initial_title, issue_url,
};
use meguri::config::Config;
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_READY};

fn params<'a>(text: &'a str, labels: &'a [&'a str]) -> AddParams<'a> {
    AddParams {
        text,
        labels,
        repo_slug: "me/repo",
    }
}

#[tokio::test]
async fn flags_apply_labels_at_capture() {
    let forge = Arc::new(FakeForge::default());
    let n = add_core(&*forge, params("do me", &[LABEL_READY]))
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert!(issue.has_label(LABEL_READY)); // 基準 4
}

#[tokio::test]
async fn raw_capture_body_is_byte_for_byte() {
    // The whole memo — leading/trailing whitespace and newlines — is the body.
    let forge = Arc::new(FakeForge::default());
    let memo = "  spaced\nmemo  ";
    let n = add_core(&*forge, params(memo, &[])).await.unwrap();
    assert_eq!(forge.get_issue(n).await.unwrap().body, memo);
}

#[test]
fn github_memo_validates_trimmed_but_returns_verbatim() {
    // The cmd_add entry point must judge emptiness on a trimmed view only:
    // the memo it hands to add_github is the original text, byte-for-byte,
    // or the verbatim guarantee (ADR 0006 原則2) breaks before add_core.
    assert_eq!(github_memo(Some("  raw memo\n")).unwrap(), "  raw memo\n");
    assert_eq!(github_memo(Some("memo")).unwrap(), "memo");
    // No memo, or only whitespace, is still rejected.
    assert!(github_memo(None).is_err());
    assert!(github_memo(Some("")).is_err());
    assert!(github_memo(Some("  \n\t ")).is_err());
}

#[test]
fn issue_url_is_composed_from_slug_and_number() {
    assert_eq!(
        issue_url("owner/repo", 123),
        "https://github.com/owner/repo/issues/123"
    );
}

#[test]
fn initial_title_takes_first_line_and_truncates() {
    assert_eq!(initial_title("  short memo \n more"), "short memo");
    let long = "あ".repeat(100);
    let t = initial_title(&long);
    assert_eq!(t.chars().count(), 72);
    assert!(t.ends_with('…'));
}

#[test]
fn infer_project_respects_path_boundaries() {
    // Two sibling repos whose names share a prefix: /repo must not match /repo2.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let repo2 = tmp.path().join("repo2");
    let nested = repo.join("src/deep");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&repo2).unwrap();

    let cfg: Config = toml::from_str(&format!(
        r#"
[[projects]]
id = "a"
repo_path = "{}"
repo_slug = "me/a"

[[projects]]
id = "b"
repo_path = "{}"
repo_slug = "me/b"
"#,
        repo.display(),
        repo2.display(),
    ))
    .unwrap();

    // cwd inside repo → project a (not b, despite the shared prefix).
    assert_eq!(infer_project(&cfg, None, &nested).unwrap().id, "a");
    // cwd inside repo2 → project b.
    assert_eq!(infer_project(&cfg, None, &repo2).unwrap().id, "b");
    // Explicit --project always wins.
    assert_eq!(infer_project(&cfg, Some("b"), &nested).unwrap().id, "b");
    // cwd under none, multiple projects → ambiguous error.
    assert!(infer_project(&cfg, None, tmp.path()).is_err());
}

/// One github-mode and one local-mode project for the flag↔mode checks.
fn two_mode_config() -> Config {
    toml::from_str(
        r#"
[[projects]]
id = "gh"
repo_path = "/tmp/gh"
repo_slug = "me/gh"

[[projects]]
id = "loc"
repo_path = "/tmp/loc"
mode = "local"
"#,
    )
    .unwrap()
}

#[test]
fn add_flags_are_checked_against_the_mode() {
    let cfg = two_mode_config();
    let (gh, local) = (&cfg.projects[0], &cfg.projects[1]);
    check_add_flags(gh, true, false).unwrap();
    // --file is a local-mode option.
    assert!(check_add_flags(gh, false, true).is_err());
    // local mode: --ready is a github-mode option; --file is fine.
    assert!(check_add_flags(local, true, false).is_err());
    check_add_flags(local, false, true).unwrap();
}
