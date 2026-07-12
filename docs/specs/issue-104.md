# issue-104 spec — meguri top: 専用 workspace に画面を構成し起動時に attach(+ ペイン解決を panes テーブルへ)

`meguri top` は herdr を正しく選んでいる(attach hint は `herdr workspace focus …`)のに、
ユーザーには「herdr を使っていない」ように見える。診断で原因は 2 つに切り分いた:

1. **画面が herdr 側に立たず attach もしない。** `ensure_dashboard` は既存 meguri
   workspace にタブを 1 個足すだけで、status ヘッダは `cmd_top` を実行した端末に
   print されるだけ。ユーザーが自分で attach hint を打つまで画面に届かない。
2. **バグ: ペイン解決が古い run 記録を見ている。** `top_refresh`(`src/app.rs`)は
   `run.mux_pane_id`(run 開始時のスナップショット)でペインを解決する。実機では
   その id は herdr に無く(`pane_not_found`)、panes テーブル側の id が実在した。
   結果 `pane_alive` が全 false → 何もタイルされず AGENT 列は全行 `unknown`。

この spec の決定と設計背景は **ADR 0005**(本 PR 同梱)に置いた。以下は実装の輪郭。

## 決定(要約 / 詳細は ADR 0005)

1. `meguri top` は **専用 workspace**(label `"<session>:top"`)を herdr に構成する。
   agent ペインの workspace(`<session>`)とは別の器。
2. status ヘッダは呼び出し元 stdout ではなく **ダッシュボードの status ペイン内**で
   描画する(内部ループを status ペインで `pane run`)。
3. `meguri top` は起動時にその workspace へ **`exec` attach** する(`cmd_attach` と同型)。
4. ペインは run 記録ではなく **panes テーブル**(`resolve_attach_pane` と同じ優先順位)で
   解決する。
5. tmux フォールバックは **専用 session** を対応物にする。

## 変更箇所

### 1. `src/app.rs` — `cmd_top` を「セットアップ + attach」役に、ループを内部化

- `cmd_top`(外側 = ユーザーが打つ):
  1. mux 検出 → `ensure_session`(agent 用 workspace)。
  2. 専用ダッシュボード workspace + status ペインを用意し、status ペイン内で内部
     ループコマンドを走らせる(冪等: 既にあれば再利用しループを二重起動しない)。
  3. ダッシュボード workspace へ `exec` attach(`cmd_attach` の exec 部を共有)。
- 内部ループ(status ペイン内で常駐 = 現 `cmd_top` のループ本体):
  - `top_refresh` を回し、`render_top` を**自分の端末**へ in-place 描画
    (`\x1b[2J\x1b[H` は既存のまま)。attach hint は「もう一度入る」補助として残す。
  - 内部ループは隠しサブコマンド(推奨)または隠しフラグで表現し、`top` の
    public help を汚さない。**要決定**: 下記「主要な決定」参照。

### 2. `src/app.rs` — `top_refresh` のペイン解決を panes テーブルへ

- 各 active run について、`run.mux_pane_id` を直接使うのをやめ、
  `store.get_pane(&run.project_id, run.issue_number)` を優先し、無ければ run 記録に
  フォールバック(= `resolve_attach_pane` と同じ規則)。可能なら解決ロジックを
  `resolve_attach_pane` と共有する小ヘルパへ切り出す。
- mux 種別も解決したペインの `mux_kind` を使い、現 mux と異なる run はスキップ
  (現状の `rk != kind` 判定を維持)。
- 同一 issue の複数 active run が 1 ペインを共有するため、ペイン id で dedup して
  タイル/行生成が重複しないようにする。

### 3. `src/mux/mod.rs` — `Multiplexer` trait の dashboard API を workspace 単位へ

- `ensure_dashboard` の意味を「workspace 内タブ」から「専用 workspace + その中の
  タイル用タブ」へ変更。status ペインを走らせるために、返り値へ **タイル先タブ**と
  **status ペイン**(root pane)を含める(例: `Dashboard { tab: DashboardId,
  status_pane: PaneId }`)か、status ペインに argv を流す trait メソッドを足す。
  **要決定**: 下記参照。
- `dashboard_attach_command` は専用 workspace(tmux は専用 session)へ入るコマンドを返す。
- `tile_pane` は従来どおりタイル用タブへ移動(実装が変わるだけ)。

### 4. `src/mux/herdr.rs` — workspace 単位のダッシュボード

- `ensure_dashboard`: `workspace list` から `"<session>:top"` を探し、無ければ
  `workspace create --label "<session>:top" --no-focus`。その workspace 内に
  タイル用タブ + status ペインを用意(冪等)。
- status ペインで内部ループを `pane run`(`spawn_pane` の `pane run` 経路を再利用)。
- `tile_pane`: `pane move --tab <dashboard tab> --split …`。**要検証**: workspace を
  またぐ `pane move` の可否(ADR 0005 の Consequences 参照)。
- `dashboard_attach_command`: `herdr workspace focus "<top ws>" ; herdr`。

### 5. `src/mux/tmux.rs` — 専用 session のダッシュボード

- `ensure_dashboard`: ダッシュボード専用 session(例 `"<session>-top"`)を
  `has-session` / `new-session -d` で用意し、status ペインで内部ループを起動。
- `tile_pane`: 既存の `join-pane` + `select-layout tiled`(移動先が専用 session になる)。
- `dashboard_attach_command`: `tmux attach -t "<top session>"`。

### 6. `src/mux/fake.rs` — テスト用 mux を新 API に追随

- 変更後の `ensure_dashboard` / status ペイン起動 / attach コマンドを実装し、
  既存の `tiled_panes()` などの検査 API を維持。

## 受け入れ基準

1. `meguri top` を実行すると herdr に `"<session>:top"` workspace が(冪等に)でき、
   status ペイン + タイルされた agent ペインで画面が構成される。
2. `meguri top` は実行した端末を専用 workspace へ `exec` attach し、hint を人手で
   打たずに画面が見える。2 回目の実行はループを二重起動せず attach だけ行う。
3. status ヘッダは status ペイン内に描画される(呼び出し元 stdout には出さない)。
4. active run のペインは panes テーブル優先で解決され、panes テーブルに実在する
   ペインがタイルされる。古い run 記録由来の `pane_not_found` で全行 `unknown` に
   ならない(#104 背景の再現ケースが解消)。
5. tmux フォールバックでも専用 session + `tmux attach` で同等の画面が立つ。
6. `render_top` / heartbeat 周りの既存ユニットテストは維持され、ペイン解決の
   優先順位と dedup を検査するテストを `fake.rs` ベースで追加する。
7. `cargo test` / `cargo clippy` が通り、README の `meguri top` 記述(L81 付近)を
   新挙動(専用 workspace へ attach)に更新する。

## 主要な決定(実装前にレビューで詰める点)

- **内部ループの表現**: 隠しサブコマンド(例 `meguri __top-status`)推奨か、`top` の
  隠しフラグ(例 `--_render`)か。前者は public help を汚さず責務が明快。後者は
  引数の共有が楽。→ 推奨: 隠しサブコマンド。
- **status ペインへ argv を流す口**: `ensure_dashboard` が argv を受け取り内部で
  `pane run` する案か、`Dashboard { tab, status_pane }` を返して app 側が既存の
  pane-run 経路で流す案か。trait の抽象度と tmux/fake 対応の手数で選ぶ。
- **workspace またぎ `pane move` の可否**: 不可なら、タイル用タブを agent workspace
  側に置き attach 先だけ切り替える等の代替(ADR 0005 参照)。実装初手で要検証。
- **ラベルの由来**: 現 `TOP_DASHBOARD_LABEL = "meguri:top"` 定数を
  `"<session>:top"`(config の session 依存)に一般化するか、定数のままにするか。
  → 推奨: session 依存(project/workspace 分離の将来と整合)。

## 非対象

- プロジェクトごとの workspace 分離(別 issue)。本 spec はレイアウトが将来それと
  衝突しないことだけ担保する。
- ダッシュボードのレイアウト最適化(タイルの並び順・比率チューニング)。
