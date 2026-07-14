//! `meguri add` (issue #120): capture-first intake with a best-effort refine.
//! The refine step is injected as a fake [`Refiner`] so these run without an
//! agent CLI; the forge is a `FakeForge`. Covers capture, refine write-back,
//! the verbatim footer, the race guard, labels, and cwd project inference.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use meguri::app::{
    AddParams, add_core, compose_refined_body, infer_project, initial_title, issue_url,
};
use meguri::config::Config;
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_PLAN};
use meguri::refine::{Refined, Refiner};

/// Always returns a fixed refined issue.
struct FixedRefiner(Refined);
#[async_trait]
impl Refiner for FixedRefiner {
    async fn refine(&self, _t: &str, _p: &Path, _l: Option<&str>) -> anyhow::Result<Refined> {
        Ok(self.0.clone())
    }
}

/// Simulates every refine failure mode (CLI absent, non-zero exit, parse
/// error, timeout, Ctrl-C) as one `Err`.
struct FailingRefiner;
#[async_trait]
impl Refiner for FailingRefiner {
    async fn refine(&self, _t: &str, _p: &Path, _l: Option<&str>) -> anyhow::Result<Refined> {
        anyhow::bail!("agent CLI not found")
    }
}

/// Simulates a human editing the issue body inside the refine window, so the
/// race guard should decline to overwrite.
struct EditingRefiner {
    forge: Arc<FakeForge>,
    number: i64,
    refined: Refined,
}
#[async_trait]
impl Refiner for EditingRefiner {
    async fn refine(&self, _t: &str, _p: &Path, _l: Option<&str>) -> anyhow::Result<Refined> {
        self.forge
            .update_issue_body(self.number, "a human rewrote this")
            .await
            .unwrap();
        Ok(self.refined.clone())
    }
}

fn params<'a>(text: &'a str, labels: &'a [&'a str]) -> AddParams<'a> {
    AddParams {
        text,
        labels,
        repo_slug: "owner/repo",
        repo_path: Path::new("/tmp"),
        language: None,
    }
}

fn refined(title: &str, body: &str) -> Refined {
    Refined {
        title: title.into(),
        body: body.into(),
    }
}

#[tokio::test]
async fn capture_is_unlabeled_and_refine_restructures_with_verbatim_memo() {
    let forge = Arc::new(FakeForge::default());
    let memo = "ログイン後のリダイレクトが変";
    let r = FixedRefiner(refined(
        "ログイン後のリダイレクト先が意図しないページになる",
        "## 症状\nリダイレクトがおかしい\n## 期待動作\n正しい遷移",
    ));
    let n = add_core(&*forge, params(memo, &[]), Some(&r), None)
        .await
        .unwrap();

    let issue = forge.get_issue(n).await.unwrap();
    // Refined title replaced the raw one-liner.
    assert_eq!(
        issue.title,
        "ログイン後のリダイレクト先が意図しないページになる"
    );
    // Body carries the structure...
    assert!(issue.body.contains("## 症状"));
    // ...and the original memo verbatim in its own section (基準 2).
    assert!(issue.body.contains("## 原文メモ"));
    assert!(issue.body.contains(memo));
    // Default capture is unlabeled = untriaged (基準 5 premise).
    assert!(issue.labels.is_empty());
}

#[tokio::test]
async fn raw_capture_never_refines() {
    // --raw is modeled as "no refiner"; the issue stays exactly as captured.
    let forge = Arc::new(FakeForge::default());
    let memo = "cleaner のレポートに stale ブランチが出ない";
    let n = add_core(&*forge, params(memo, &[]), None, None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert_eq!(issue.title, memo);
    assert_eq!(issue.body, memo);
}

#[tokio::test]
async fn refine_failure_leaves_issue_raw_and_reports_success() {
    let forge = Arc::new(FakeForge::default());
    let memo = "add コマンドが欲しい";
    // add_core returns Ok (capture succeeded) even though refine failed (基準 3).
    let n = add_core(&*forge, params(memo, &[]), Some(&FailingRefiner), None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert_eq!(issue.title, memo);
    assert_eq!(issue.body, memo);
}

#[tokio::test]
async fn flags_apply_labels_at_capture() {
    let forge = Arc::new(FakeForge::default());
    let n = add_core(&*forge, params("plan me", &[LABEL_PLAN]), None, None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert!(issue.has_label(LABEL_PLAN)); // 基準 4
}

#[tokio::test]
async fn refine_guard_keeps_human_edit() {
    let forge = Arc::new(FakeForge::default());
    let memo = "guard を検証する";
    let editor = EditingRefiner {
        forge: forge.clone(),
        number: 1,
        refined: refined("AI title", "AI body"),
    };
    let n = add_core(&*forge, params(memo, &[]), Some(&editor), None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    // The human edit stands; refine did not overwrite title or body (基準 8).
    assert_eq!(issue.title, memo);
    assert_eq!(issue.body, "a human rewrote this");
    assert!(!issue.body.contains("AI body"));
}

#[tokio::test]
async fn raw_capture_body_is_byte_for_byte() {
    // The whole memo — leading/trailing whitespace and newlines — is the body.
    let forge = Arc::new(FakeForge::default());
    let memo = "  spaced\nmemo  ";
    let n = add_core(&*forge, params(memo, &[]), None, None)
        .await
        .unwrap();
    assert_eq!(forge.get_issue(n).await.unwrap().body, memo);
}

#[tokio::test]
async fn refine_footer_preserves_memo_whitespace() {
    let forge = Arc::new(FakeForge::default());
    let memo = "  行頭スペースと\n改行を保つ  ";
    let r = FixedRefiner(refined("整った題", "整った本文"));
    let n = add_core(&*forge, params(memo, &[]), Some(&r), None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert!(
        issue
            .body
            .ends_with("## 原文メモ\n  行頭スペースと\n改行を保つ  ")
    );
}

#[tokio::test]
async fn write_back_body_failure_leaves_issue_raw() {
    // A forge hiccup on the body write happens first → the title is never
    // touched → the issue stays fully raw, and add_core still returns Ok.
    let forge = Arc::new(FakeForge::default());
    let memo = "capture me";
    forge.update_body_errors.lock().unwrap().insert(1);
    let r = FixedRefiner(refined("AI title", "AI body"));
    let n = add_core(&*forge, params(memo, &[]), Some(&r), None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert_eq!(issue.title, memo);
    assert_eq!(issue.body, memo);
}

#[tokio::test]
async fn write_back_title_failure_keeps_a_coherent_issue() {
    // Body is written before title, so a title-write failure never leaves a
    // refined title on a raw body: worst case is a refined body (with the
    // verbatim memo) under the raw one-line title. add_core returns Ok.
    let forge = Arc::new(FakeForge::default());
    let memo = "capture me";
    forge.update_title_errors.lock().unwrap().insert(1);
    let r = FixedRefiner(refined("AI title", "## 症状\nx"));
    let n = add_core(&*forge, params(memo, &[]), Some(&r), None)
        .await
        .unwrap();
    let issue = forge.get_issue(n).await.unwrap();
    assert_eq!(issue.title, memo); // raw title stands
    assert!(issue.body.contains("## 症状")); // refined body applied
    assert!(issue.body.contains(memo)); // original memo still present
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
fn verbatim_footer_keeps_the_original_byte_for_byte() {
    // The refined scaffold is trimmed, but the original memo is embedded as-is.
    let body = compose_refined_body("  ## 症状\nx  ", "  raw memo\n");
    assert!(body.starts_with("## 症状\nx"));
    assert!(body.ends_with("## 原文メモ\n  raw memo\n"));
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
