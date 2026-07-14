//! `meguri add-project` (issue #196): the pure planning/validation logic and
//! the append mechanism (comment-preserving, injection-safe). The gh repo
//! creation and clone are network/gh-bound and out of scope here; they live
//! behind free functions (`gh::create_repo` / `gitops::ensure_bare_clone`).

use std::io::Write;

use meguri::app::plan_add_project;
use meguri::config::{self, Config, ProjectDraft};

fn cfg_from(raw: &str) -> Config {
    toml::from_str(raw).expect("fixture config parses")
}

fn empty_cfg() -> Config {
    cfg_from("")
}

// ---- slug validation ----

#[test]
fn valid_slugs_are_accepted() {
    for slug in ["owner/repo", "a-b.c/d_e.f", "Org123/Repo-1"] {
        config::validate_repo_slug(slug).unwrap_or_else(|e| panic!("{slug} should pass: {e}"));
    }
}

#[test]
fn slug_rejects_shape_and_traversal_and_injection_chars() {
    // Wrong shape / extra separators / empty halves.
    for slug in [
        "owner",
        "owner/repo/extra",
        "/repo",
        "owner/",
        "owner//repo",
    ] {
        assert!(
            config::validate_repo_slug(slug).is_err(),
            "{slug} must be rejected"
        );
    }
    // Path traversal: a component that is exactly `.` or `..` (the regex-only
    // character check would let these through — the per-component gate catches them).
    for slug in ["./repo", "owner/..", "../repo", "owner/."] {
        assert!(
            config::validate_repo_slug(slug).is_err(),
            "{slug} must be rejected"
        );
    }
    // Injection / disallowed characters.
    for slug in [
        "owner/re po",
        "owner/re\"po",
        "owner/re\npo",
        "owner\\repo",
        "owner/re;po",
    ] {
        assert!(
            config::validate_repo_slug(slug).is_err(),
            "{slug:?} must be rejected"
        );
    }
}

// ---- planning: id derivation, collision, mode ----

#[test]
fn github_plan_derives_id_from_repo_half() {
    let plan = plan_add_project(&empty_cfg(), Some("owner/myrepo"), None, None).unwrap();
    assert_eq!(plan.draft.id, "myrepo");
    assert_eq!(plan.draft.repo_slug.as_deref(), Some("owner/myrepo"));
    assert_eq!(plan.draft.repo_path, None);
    assert_eq!(plan.draft.mode, None); // default github
    assert!(!plan.is_local);
}

#[test]
fn explicit_id_wins_and_is_validated() {
    let plan = plan_add_project(&empty_cfg(), Some("owner/repo"), Some("custom"), None).unwrap();
    assert_eq!(plan.draft.id, "custom");
    // A structurally bad id is rejected.
    assert!(plan_add_project(&empty_cfg(), Some("owner/repo"), Some("a/b"), None).is_err());
}

#[test]
fn local_plan_needs_absolute_path_and_sets_mode() {
    let plan = plan_add_project(&empty_cfg(), None, None, Some("/abs/proj")).unwrap();
    assert!(plan.is_local);
    assert_eq!(plan.draft.id, "proj");
    assert_eq!(plan.draft.mode.as_deref(), Some("local"));
    assert_eq!(plan.draft.repo_path.as_deref(), Some("/abs/proj"));
    assert_eq!(plan.draft.repo_slug, None);
    // A relative path is rejected.
    assert!(plan_add_project(&empty_cfg(), None, None, Some("rel/path")).is_err());
}

#[test]
fn collision_on_id_or_slug_is_rejected() {
    let cfg = cfg_from("[[projects]]\nid = \"repo\"\nrepo_slug = \"owner/repo\"\n");
    // Same derived id.
    assert!(plan_add_project(&cfg, Some("owner/repo"), None, None).is_err());
    // Different id but same slug.
    assert!(plan_add_project(&cfg, Some("owner/repo"), Some("other"), None).is_err());
    // Same repo in different case is the same GitHub repo — also rejected.
    assert!(plan_add_project(&cfg, Some("Owner/Repo"), Some("other"), None).is_err());
    // Different id and different slug is fine.
    assert!(plan_add_project(&cfg, Some("owner/fresh"), Some("fresh"), None).is_ok());
}

// ---- append: comment-preserving, injection-safe ----

fn write_temp(contents: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn append_preserves_existing_comments_and_adds_one_project() {
    let original = "# hand-written comment — keep me\n\
                    [[projects]]\nid = \"existing\"\nrepo_slug = \"o/existing\"\n";
    let f = write_temp(original);
    let draft = ProjectDraft {
        id: "fresh".into(),
        repo_slug: Some("o/fresh".into()),
        repo_path: None,
        mode: None,
    };
    config::append_project(f.path(), &draft).unwrap();

    // The comment (and the original project) survive; exactly one project is added.
    let raw = std::fs::read_to_string(f.path()).unwrap();
    assert!(
        raw.contains("# hand-written comment — keep me"),
        "comment must survive"
    );
    let cfg = Config::load_from(f.path()).unwrap();
    assert_eq!(cfg.projects.len(), 2);
    assert!(cfg.projects.iter().any(|p| p.id == "existing"));
    assert!(cfg.projects.iter().any(|p| p.id == "fresh"));
}

#[test]
fn append_serializes_values_no_injection() {
    // A local repo_path carrying quotes / newline / backslash must round-trip
    // exactly and must not break the TOML structure or inject new keys.
    let original = "[[projects]]\nid = \"keep\"\nrepo_path = \"/kept\"\nmode = \"local\"\n";
    let f = write_temp(original);
    let nasty = "/tmp/a\"b\nid = \"injected\"\nc\\d";
    let draft = ProjectDraft {
        id: "evil".into(),
        repo_slug: None,
        repo_path: Some(nasty.into()),
        mode: Some("local".into()),
    };
    config::append_project(f.path(), &draft).unwrap();

    let cfg = Config::load_from(f.path()).unwrap();
    assert_eq!(cfg.projects.len(), 2, "no injected extra project");
    let evil = cfg.projects.iter().find(|p| p.id == "evil").unwrap();
    assert_eq!(
        evil.repo_path.as_deref().unwrap().to_str().unwrap(),
        nasty,
        "value round-trips verbatim (escaped, not injected)"
    );
    // The attempted injected id never became a project.
    assert!(!cfg.projects.iter().any(|p| p.id == "injected"));
    assert!(cfg.projects.iter().any(|p| p.id == "keep"));
}

// ---- init template integrity (spec issue-196, ADR 0019) ----

#[test]
fn init_template_parses_to_zero_projects() {
    let f = write_temp(config::INIT_TEMPLATE);
    let cfg = Config::load_from(f.path()).unwrap();
    assert!(
        cfg.projects.is_empty(),
        "fresh `meguri init` config must have no live [[projects]] stub"
    );
}

#[test]
fn init_then_add_project_yields_exactly_one_project() {
    let f = write_temp(config::INIT_TEMPLATE);
    let draft = ProjectDraft {
        id: "repo".into(),
        repo_slug: Some("owner/repo".into()),
        repo_path: None,
        mode: None,
    };
    config::append_project(f.path(), &draft).unwrap();
    let cfg = Config::load_from(f.path()).unwrap();
    assert_eq!(cfg.projects.len(), 1, "no dummy owner/repo left behind");
    assert_eq!(cfg.projects[0].id, "repo");
    assert_eq!(cfg.projects[0].repo_slug.as_deref(), Some("owner/repo"));
}
