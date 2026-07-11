# Spec: `meguri serve` — web ダッシュボード Phase 1(読み取り専用 MVP) — issue #36

## ゴール

run の状況(一覧、interaction state、イベントトレイル、ペイン出力)をブラウザで
一覧できる読み取り専用ダッシュボードを `meguri serve` として追加する。最重要価値は
**`awaiting_human` の run に一目で気づけること**。watch が動いていなくても
(`meguri run` 単発運用でも)UI が使えること。

制御操作(pause/stop 等の POST)、SSE ライブ更新、認証は Phase 2 以降でスコープ外。

## キーデシジョン

### D1. serve は Store + Config だけを持つ独立プロセス(watch への IPC なし)→ ADR 0002

CLI の pause/stop が DB の `desired_state` 経由で watch と協調している(`src/app.rs`)
のと同型に、serve も `Config::load` + `Store::open` するだけの独立リーダーとする。
watch プロセスへの接続・IPC は持たない(ADR 0001 の方針の延長 — ADR 0002 に記録)。

将来 `meguri watch --serve` で同居起動できるよう、サーバー本体は
`src/server/mod.rs` の `pub async fn serve(store: Store, config: Config, listener: TcpListener)`
(+ テスト用に `pub fn router(state: AppState) -> Router`)として書き、
`app::cmd_serve` は bind + 呼び出しだけの薄いラッパにする。

### D2. 依存追加は `axum` のみ。テスト用に `tower`(util)+ `http-body-util` を dev-deps に

- `axum = "0.8"`(tokio は導入済み)。静的アセットは `include_str!` 埋め込みなので
  `tower-http` は不要。
- ハンドラテストは `Router` に `tower::ServiceExt::oneshot` でリクエストを流す標準形。
  `tower = { version = "0.5", features = ["util"] }` と `http-body-util` を
  dev-dependencies に追加する。

### D3. config に `[server]` セクション(port / bind)。CLI フラグが上書き

```toml
[server]
port = 8607
bind = "127.0.0.1"
```

- `meguri serve [--port N] [--bind ADDR]`。デフォルトは `127.0.0.1:8607`。
- 認証なしのため、bind が loopback 以外なら起動時に **警告を出して続行**
  (拒否はしない — LAN 内利用は利用者判断。トークン認証は Phase 4)。

### D4. watch 生死はハートビート専用テーブル(events には書かない)

- migration `0002_heartbeats.sql`:
  `CREATE TABLE heartbeats (name TEXT PRIMARY KEY, ts TEXT NOT NULL);`
- `Scheduler::watch` のループが tick ごとに `('watch', now())` を UPSERT
  (`Store::heartbeat(name)` / `Store::latest_heartbeat(name)` を追加)。
- events に書く案は不採番: tick ごと(既定 60 秒)に行が無限に増え、イベントトレイルの
  ノイズにもなる。1 行 UPSERT なら肥大しない。
- 鮮度判定はサーバー側(config の `scheduler.poll_interval_secs` を知っているため):
  `age < poll_interval * 2 + 30s` なら alive。API は `last_heartbeat`(ts)と
  `alive`(bool)の両方を返し、UI は表示するだけ。

### D5. mux 解決を注入可能にして FakeMux でハンドラをテストする

tail ハンドラは `run.mux_kind` / `run.mux_pane_id` から mux を復元して
`read_tail` / `agent_state` / `attach_command` を呼ぶ(`cmd_logs` と同じ経路)。
本番は `mux::from_kind` だが、テストで `FakeMux` を差し込めるよう `AppState` に
リゾルバを持たせる:

```rust
pub type MuxResolver =
    Arc<dyn Fn(&str, &str) -> anyhow::Result<Arc<dyn Multiplexer>> + Send + Sync>;

pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub mux_resolver: MuxResolver, // 本番: mux::from_kind
}
```

mux 復元失敗・pane 死亡は 500 にせず `pane_alive: false` / `agent_state: "unknown"` で
正常応答する(`cmd_logs` が黙って省略するのと同じ寛容さ)。

### D6. Store / 型の拡張

| 型 | 変更 |
|---|---|
| `RunRecord` | `Serialize` derive。`started_at` / `finished_at` カラムを読む(schema にはあるが未マップ — 経過時間表示に必要) |
| `DesiredState` | `Serialize` derive(issue は「enum 側は付与済み」とするが `RunStatus` / `InteractionState` のみ。`rename_all = "snake_case"` で揃える) |
| `EventRecord` | `Serialize` derive + `id: i64` を追加(カーソルに必要)。`Store::events_for_run_after(run_id, after_id, limit)` を追加(`WHERE id > ?` を id 昇順で) |
| `TurnRecord`(新規) | `turns` テーブルの読み出し用 struct + `Store::list_turns(run_id)`(turn_no 昇順) |
| `AgentState` | `as_str()`(`"working"` 等)を追加して tail レスポンスに載せる |

`checkpoint_json` はそのまま文字列でシリアライズされる(UI は使わないだけ)。
隠したくなったら `#[serde(skip)]` だが Phase 1 では素通しで良い(localhost 限定)。

### D7. UI は単一 HTML + vanilla JS を `include_str!`、hash ルーティング、3 秒ポーリング

- `src/server/ui.html` 1 ファイル(CSS/JS インライン)。node ツールチェーンなし。
- `GET /` で配信。`#/` = ダッシュボード、`#/runs/<id>` = run 詳細の hash ルーティング。
- `setInterval` 3 秒でポーリング(events は `after=<最後の id>` の差分取得)。
- ダッシュボード: awaiting_human 数を最上部で強調(> 0 なら目立つ色 + document.title
  にも件数)、active 数 / watch 生死のサマリー、runs テーブル
  (`ps` のカラム + issue タイトル + 経過時間)。`awaiting_human` 行はハイライト。
  「終了 run も表示」トグル(`?all=true` 相当)。
- run 詳細: イベントトレイル、ペインテールの端末風表示(黒背景 `<pre>` +
  agent_state チップ)、turns 履歴、attach コマンドのコピー表示。

## HTTP API

| エンドポイント | レスポンス(概形) |
|---|---|
| `GET /` | 埋め込み HTML |
| `GET /api/status` | `{ projects: [{id, repo_slug}], watch: {last_heartbeat, alive}, active_runs, awaiting_human }` |
| `GET /api/runs?all=true` | `[RunRecord…]`(パラメータなし = active のみ、`list_runs` と同じ規約) |
| `GET /api/runs/:id` | `{ run: RunRecord, turns: [TurnRecord…] }` |
| `GET /api/runs/:id/events?after=<id>&limit=<n>` | `{ events: [{id, ts, kind, data}…] }`(id 昇順、既定 limit 200) |
| `GET /api/runs/:id/tail?lines=50` | `{ lines: […], agent_state, attach_command, pane_alive }` |

- run 解決は `find_run`(prefix / issue 番号も効く)を使い、見つからなければ
  404 + `{"error": "..."}`。
- `/api/status` のカウントは `list_runs(true)` をハンドラ内で数えるだけ
  (専用カウントクエリは不要な規模)。

## 触るファイル

| ファイル | 変更 |
|---|---|
| `Cargo.toml` | `axum` 追加; dev: `tower`(util), `http-body-util` |
| `src/cli.rs` / `src/main.rs` | `Serve { port, bind }` サブコマンド + dispatch |
| `src/app.rs` | `cmd_serve`(config/store を組んで `server::serve` へ) |
| `src/config.rs` | `ServerConfig`(port / bind)追加 |
| `src/server/mod.rs`(新規) | `AppState` / `router()` / `serve()` / ハンドラ |
| `src/server/ui.html`(新規) | 単一ページ UI(`include_str!`) |
| `src/store/mod.rs` | migration 0002 登録、heartbeat 読み書き |
| `src/store/migrations/0002_heartbeats.sql`(新規) | heartbeats テーブル |
| `src/store/runs.rs` | `RunRecord` 拡張(Serialize, started/finished_at)、`TurnRecord` + `list_turns` |
| `src/events.rs` | `EventRecord.id` + Serialize、`events_for_run_after` |
| `src/engine/scheduler.rs` | tick ごとに `store.heartbeat("watch")` |
| `src/mux/mod.rs` | `AgentState::as_str()` |
| `README.md` / `README.ja.md` | `meguri serve` 節追加 |
| `tests/server_test.rs`(新規) | 下記 |

## テスト

- ハンドラ(`tests/server_test.rs`): in-memory `Store` + `FakeMux` を注入した
  `router()` に `oneshot` で:
  - `/api/status` — awaiting_human / active のカウント、heartbeat なし → `alive: false`、
    新鮮な heartbeat → `alive: true`、古い heartbeat → `alive: false`
  - `/api/runs` — active のみ / `all=true`
  - `/api/runs/:id` — run + turns(`begin_turn`/`finish_turn` で仕込む)、未知 id は 404
  - `/api/runs/:id/events` — `after` カーソルで差分だけ返る
  - `/api/runs/:id/tail` — FakePane の tail / state / attach が返る;
    pane なし・死亡で `pane_alive: false`(500 にならない)
  - `/` が HTML を返す
- store unit: heartbeat UPSERT roundtrip、`events_for_run_after`、`list_turns`
- scheduler: tick が heartbeat を更新する(既存 `scheduler_test.rs` に追加)
- config: `[server]` デフォルト(8607 / 127.0.0.1)と上書き
- UI は自動テストしない(受け入れチェックリストで手動確認)

## 受け入れ条件(issue から)

- [ ] `meguri serve` で起動し、ブラウザで run 一覧・run 詳細(イベント + ペインテール)が見られる
- [ ] `awaiting_human` の run がダッシュボードで一目で分かる
- [ ] watch が動いていない状態でも serve 単体で過去 run を閲覧できる
- [ ] `cargo test` パス。API ハンドラは in-memory Store + fake mux でテスト

## スコープ外(follow-up issue)

- Phase 2: 制御操作(pause/resume/stop/takeover/handback の POST)— `app.rs` のロジック共有化を伴う
- Phase 3: SSE によるライブ更新、テール追従
- Phase 4: UI からの issue enqueue、herdr ソケット直結の xterm.js アタッチ、トークン認証
