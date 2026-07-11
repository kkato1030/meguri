//! `meguri serve`: read-only web dashboard over the shared sqlite store.
//!
//! An independent reader in the spirit of the CLI (ADR 0002): no IPC to
//! watch — everything comes from `Store`, except pane tails, which are read
//! through the mux resolved from the run record (same path as `meguri logs`).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::config::Config;
use crate::mux::{self, AgentState, Multiplexer, PaneId};
use crate::store::{InteractionState, Store, parse_ts};

/// Resolves `(mux_kind, session)` to a live multiplexer. Production uses
/// `mux::from_kind`; tests inject a `FakeMux`.
pub type MuxResolver =
    Arc<dyn Fn(&str, &str) -> anyhow::Result<Arc<dyn Multiplexer>> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub mux_resolver: MuxResolver,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/api/status", get(status))
        .route("/api/runs", get(runs))
        .route("/api/runs/{id}", get(run_detail))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/tail", get(run_tail))
        .with_state(state)
}

/// Run the dashboard until the process dies. Kept separate from `cmd_serve`
/// so a future `meguri watch --serve` can spawn it next to the scheduler.
pub async fn serve(store: Store, config: Config, listener: TcpListener) -> anyhow::Result<()> {
    let state = AppState {
        store,
        config,
        mux_resolver: Arc::new(mux::from_kind),
    };
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// JSON error envelope: every non-2xx response is `{"error": "..."}`.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn find_run(state: &AppState, needle: &str) -> Result<crate::store::RunRecord, ApiError> {
    state
        .store
        .find_run(needle)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no run matches {needle:?}")))
}

async fn ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

/// Freshness window for the watch heartbeat: two poll ticks plus slack, so a
/// single slow tick doesn't flap the indicator.
fn heartbeat_alive(ts: &str, poll_interval_secs: u64) -> bool {
    let Some(then) = parse_ts(ts) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    now.saturating_sub(then) < poll_interval_secs * 2 + 30
}

async fn status(State(state): State<AppState>) -> ApiResult {
    let runs = state.store.list_runs(true)?;
    let awaiting = runs
        .iter()
        .filter(|r| r.interaction_state == Some(InteractionState::AwaitingHuman))
        .count();
    let heartbeat = state.store.latest_heartbeat("watch")?;
    let alive = heartbeat
        .as_deref()
        .map(|ts| heartbeat_alive(ts, state.config.scheduler.poll_interval_secs))
        .unwrap_or(false);
    let projects: Vec<Value> = state
        .config
        .projects
        .iter()
        .map(|p| json!({ "id": p.id, "repo_slug": p.repo_slug }))
        .collect();
    Ok(Json(json!({
        "projects": projects,
        "watch": { "last_heartbeat": heartbeat, "alive": alive },
        "active_runs": runs.len(),
        "awaiting_human": awaiting,
    })))
}

#[derive(Deserialize)]
struct RunsQuery {
    #[serde(default)]
    all: bool,
}

async fn runs(State(state): State<AppState>, Query(q): Query<RunsQuery>) -> ApiResult {
    let runs = state.store.list_runs(!q.all)?;
    Ok(Json(json!(runs)))
}

async fn run_detail(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let run = find_run(&state, &id)?;
    let turns = state.store.list_turns(&run.id)?;
    Ok(Json(json!({ "run": run, "turns": turns })))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}

async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> ApiResult {
    let run = find_run(&state, &id)?;
    let events = state
        .store
        .events_for_run_after(&run.id, q.after, q.limit.unwrap_or(200))?;
    Ok(Json(json!({ "events": events })))
}

#[derive(Deserialize)]
struct TailQuery {
    lines: Option<usize>,
}

/// Pane tail + agent state. A missing pane, an unresolvable mux, or a dead
/// pane is normal life (finished run, watch on another host) — respond
/// `pane_alive: false` rather than an error, like `cmd_logs` does.
async fn run_tail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TailQuery>,
) -> ApiResult {
    let run = find_run(&state, &id)?;
    let lines = q.lines.unwrap_or(50);

    let mut resp = json!({
        "lines": [],
        "agent_state": AgentState::Unknown.as_str(),
        "attach_command": Value::Null,
        "pane_alive": false,
    });
    if let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        && let Ok(mux) = (state.mux_resolver)(kind, &state.config.mux.session)
    {
        let pane = PaneId(pane.clone());
        resp["attach_command"] = json!(mux.attach_command(&pane));
        if mux.pane_alive(&pane).await.unwrap_or(false) {
            resp["pane_alive"] = json!(true);
            resp["lines"] = json!(mux.read_tail(&pane, lines).await.unwrap_or_default());
            let agent_state = mux.agent_state(&pane).await.unwrap_or(AgentState::Unknown);
            resp["agent_state"] = json!(agent_state.as_str());
        }
    }
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_freshness_window() {
        let ts = crate::store::now();
        assert!(heartbeat_alive(&ts, 60));
        assert!(!heartbeat_alive("2000-01-01T00:00:00Z", 60));
        assert!(!heartbeat_alive("garbage", 60));
    }
}
