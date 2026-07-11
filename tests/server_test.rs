//! Handler tests for `meguri serve`: an in-memory Store and an injected
//! FakeMux behind the real Router, driven with `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use meguri::config::Config;
use meguri::mux::fake::FakeMux;
use meguri::mux::{AgentState, PaneId, PaneSpec};
use meguri::server::{AppState, router};
use meguri::store::{InteractionState, RunStatus, Store};
use serde_json::Value;
use tower::ServiceExt;

fn make_router(store: Store, fake: Arc<FakeMux>) -> Router {
    let state = AppState {
        store,
        config: Config::default(),
        mux_resolver: Arc::new(move |_kind, _session| Ok(fake.clone() as _)),
    };
    router(state)
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let res = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn spawn_fake_pane(fake: &FakeMux) -> PaneId {
    use meguri::mux::Multiplexer;
    fake.spawn_pane(&PaneSpec {
        title: "t".into(),
        cwd: "/tmp".into(),
        command: vec!["true".into()],
        env: vec![],
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn status_counts_and_heartbeat_freshness() {
    let store = Store::open_in_memory().unwrap();
    let app = make_router(store.clone(), Arc::new(FakeMux::new(false)));

    // No heartbeat at all → watch is down.
    let (code, body) = get(&app, "/api/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["watch"]["alive"], false);
    assert_eq!(body["watch"]["last_heartbeat"], Value::Null);
    assert_eq!(body["active_runs"], 0);
    assert_eq!(body["awaiting_human"], 0);

    let a = store.create_run("demo", 1, "one").unwrap();
    store.create_run("demo", 2, "two").unwrap();
    let done = store.create_run("demo", 3, "three").unwrap();
    store
        .update_interaction_state(&a.id, Some(InteractionState::AwaitingHuman))
        .unwrap();
    store
        .update_run_status(&done.id, RunStatus::Succeeded, None)
        .unwrap();

    // Fresh heartbeat → alive; finished runs don't count as active.
    store.heartbeat("watch").unwrap();
    let (_, body) = get(&app, "/api/status").await;
    assert_eq!(body["watch"]["alive"], true);
    assert!(body["watch"]["last_heartbeat"].is_string());
    assert_eq!(body["active_runs"], 2);
    assert_eq!(body["awaiting_human"], 1);

    // Stale heartbeat (way past poll_interval * 2 + 30s) → down again.
    store.heartbeat_at("watch", "2020-01-01T00:00:00Z").unwrap();
    let (_, body) = get(&app, "/api/status").await;
    assert_eq!(body["watch"]["alive"], false);
}

#[tokio::test]
async fn runs_lists_active_by_default_and_all_on_request() {
    let store = Store::open_in_memory().unwrap();
    let active = store.create_run("demo", 1, "active").unwrap();
    let done = store.create_run("demo", 2, "done").unwrap();
    store
        .update_run_status(&done.id, RunStatus::Succeeded, None)
        .unwrap();
    let app = make_router(store, Arc::new(FakeMux::new(false)));

    let (code, body) = get(&app, "/api/runs").await;
    assert_eq!(code, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![active.id.as_str()]);

    let (_, body) = get(&app, "/api/runs?all=true").await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    // The serialized record speaks the CLI vocabulary (snake_case enums).
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "succeeded")
    );
}

#[tokio::test]
async fn run_detail_returns_run_with_turns_and_404_on_unknown() {
    let store = Store::open_in_memory().unwrap();
    let run = store.create_run("demo", 7, "with turns").unwrap();
    store
        .begin_turn(&run.id, "turn-1", "execute", "/tmp/p.md")
        .unwrap();
    store.finish_turn("turn-1", "success", Some("{}")).unwrap();
    store
        .begin_turn(&run.id, "turn-2", "validate-fix", "/tmp/p2.md")
        .unwrap();
    let app = make_router(store, Arc::new(FakeMux::new(false)));

    let (code, body) = get(&app, &format!("/api/runs/{}", run.id)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["run"]["id"], run.id.as_str());
    assert_eq!(body["run"]["issue_number"], 7);
    let turns = body["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["turn_no"], 1);
    assert_eq!(turns[0]["outcome"], "success");
    assert_eq!(turns[1]["outcome"], Value::Null);

    // find_run semantics: the issue number resolves to its active run.
    let (code, body) = get(&app, "/api/runs/7").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["run"]["id"], run.id.as_str());

    let (code, body) = get(&app, "/api/runs/run-nope").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("run-nope"));
}

#[tokio::test]
async fn run_events_pages_with_after_cursor() {
    let store = Store::open_in_memory().unwrap();
    let run = store.create_run("demo", 1, "t").unwrap();
    for i in 0..3 {
        store
            .emit(Some(&run.id), "test.tick", serde_json::json!({ "i": i }))
            .unwrap();
    }
    let app = make_router(store, Arc::new(FakeMux::new(false)));

    let (code, body) = get(&app, &format!("/api/runs/{}/events", run.id)).await;
    assert_eq!(code, StatusCode::OK);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["kind"], "test.tick");
    assert_eq!(events[0]["data"]["i"], 0);

    let cursor = events[1]["id"].as_i64().unwrap();
    let (_, body) = get(&app, &format!("/api/runs/{}/events?after={cursor}", run.id)).await;
    let rest = body["events"].as_array().unwrap();
    assert_eq!(rest.len(), 1, "only events past the cursor");
    assert_eq!(rest[0]["data"]["i"], 2);

    let (_, body) = get(&app, &format!("/api/runs/{}/events?limit=2", run.id)).await;
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn run_tail_reads_pane_through_injected_mux() {
    let store = Store::open_in_memory().unwrap();
    let fake = Arc::new(FakeMux::new(true));
    let pane = spawn_fake_pane(&fake).await;
    fake.set_tail(
        &pane,
        vec!["line one".into(), "line two".into(), "line three".into()],
    );
    fake.set_state(&pane, AgentState::Working);

    let run = store.create_run("demo", 1, "t").unwrap();
    store
        .update_run_mux(&run.id, "tmux", "meguri", &pane.0)
        .unwrap();
    let app = make_router(store, fake.clone());

    let (code, body) = get(&app, &format!("/api/runs/{}/tail", run.id)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["pane_alive"], true);
    assert_eq!(body["agent_state"], "working");
    assert_eq!(body["lines"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["attach_command"].as_str().unwrap(),
        format!("echo fake pane {pane}")
    );

    let (_, body) = get(&app, &format!("/api/runs/{}/tail?lines=2", run.id)).await;
    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "line two");
}

#[tokio::test]
async fn run_tail_is_forgiving_about_missing_or_dead_panes() {
    let store = Store::open_in_memory().unwrap();
    let fake = Arc::new(FakeMux::new(true));

    // No pane recorded at all.
    let bare = store.create_run("demo", 1, "no pane").unwrap();
    let app = make_router(store.clone(), fake.clone());
    let (code, body) = get(&app, &format!("/api/runs/{}/tail", bare.id)).await;
    assert_eq!(code, StatusCode::OK, "must not 500");
    assert_eq!(body["pane_alive"], false);
    assert_eq!(body["agent_state"], "unknown");
    assert_eq!(body["attach_command"], Value::Null);

    // Pane recorded but dead.
    let pane = spawn_fake_pane(&fake).await;
    let dead = store.create_run("demo", 2, "dead pane").unwrap();
    store
        .update_run_mux(&dead.id, "tmux", "meguri", &pane.0)
        .unwrap();
    fake.kill(&pane);
    let (code, body) = get(&app, &format!("/api/runs/{}/tail", dead.id)).await;
    assert_eq!(code, StatusCode::OK, "must not 500");
    assert_eq!(body["pane_alive"], false);
    assert_eq!(body["agent_state"], "unknown");
    assert!(body["lines"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn root_serves_the_embedded_ui() {
    let store = Store::open_in_memory().unwrap();
    let app = make_router(store, Arc::new(FakeMux::new(false)));

    let res = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res.headers()["content-type"].to_str().unwrap().to_string();
    assert!(content_type.starts_with("text/html"));
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("meguri"));
    assert!(html.contains("awaiting"));
}
