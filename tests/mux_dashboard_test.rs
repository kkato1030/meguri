//! Dashboard-layout contract for the `Multiplexer` trait (`meguri top`, #96):
//! `ensure_dashboard` / `tile_pane` / `dashboard_attach_command`. Exercised
//! against the fake mux so the orchestration logic is covered without a live
//! herdr/tmux; the herdr/tmux backends share the same trait surface.

use std::path::PathBuf;

use meguri::mux::fake::FakeMux;
use meguri::mux::{Multiplexer, PaneId, PaneSpec, Split};

fn spec(title: &str) -> PaneSpec {
    PaneSpec {
        title: title.into(),
        cwd: PathBuf::from("/tmp"),
        command: vec!["claude".into()],
        env: vec![],
    }
}

#[tokio::test]
async fn tiles_live_panes_and_records_order() {
    let mux = FakeMux::new(true);
    let dashboard = mux.ensure_dashboard("meguri:top").await.unwrap();
    assert_eq!(dashboard.0, "fake-dash:meguri:top");

    let a = mux.spawn_pane(&spec("a")).await.unwrap();
    let b = mux.spawn_pane(&spec("b")).await.unwrap();

    mux.tile_pane(&a, &dashboard, Split::Down).await.unwrap();
    mux.tile_pane(&b, &dashboard, Split::Down).await.unwrap();

    let tiled = mux.tiled_panes();
    assert_eq!(tiled.len(), 2);
    assert_eq!(tiled[0].0, a);
    assert_eq!(tiled[0].1, dashboard);
    assert_eq!(tiled[0].2, Split::Down);
    assert_eq!(tiled[1].0, b);
}

#[tokio::test]
async fn tiling_a_dead_pane_is_an_error() {
    let mux = FakeMux::new(true);
    let dashboard = mux.ensure_dashboard("meguri:top").await.unwrap();

    let pane = mux.spawn_pane(&spec("gone")).await.unwrap();
    mux.kill(&pane);

    assert!(mux.tile_pane(&pane, &dashboard, Split::Down).await.is_err());
    assert!(mux.tiled_panes().is_empty());

    // A never-spawned pane is likewise not tileable.
    let ghost = PaneId("fake:999".into());
    assert!(
        mux.tile_pane(&ghost, &dashboard, Split::Down)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dashboard_attach_command_names_the_dashboard() {
    let mux = FakeMux::new(true);
    let dashboard = mux.ensure_dashboard("meguri:top").await.unwrap();
    let cmd = mux.dashboard_attach_command(&dashboard);
    assert!(cmd.contains("fake-dash:meguri:top"), "got: {cmd}");
}
