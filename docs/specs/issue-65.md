# issue-65 spec — routing (2/3): 振り分け基準の継続検査(meguri stats routing + doctor 鮮度チェック)

#64 で入れた役割ベース振り分けが**古びていないか**を継続検査する。2 層構成 — 表そのものの賞味期限を見る**静的鮮度チェック**(doctor)と、実 run データで裏切られていないかを見る**成果ドリフト検知**(scheduler + stats)。設計判断は ADR 0007 に置いた(本 PR 同梱)。ここは収束のための実装仕様。

## 調査で判明した前提(この branch の現状)

- **`meguri serve` は撤去済み**(#95/#97、ADR 0002 は superseded)。ダッシュボードは端末ネイティブな `meguri top`(`src/app.rs`)。→ issue の「serve ダッシュボードの 1 ページ」は **`meguri stats routing`(CLI)+ `meguri top` ヘッダの drift 行**に読み替える(ADR 0007)。
- **役割の軸はすでにある**: `runs.loop_kind`(= `routing::resolve` がキーにする役割)と `runs.agent_profile`(#64 が spawn 時に固定)。集計は両カラムの GROUP BY で足りる。次元表は不要。
- **成果指標の素材はすでに揃っている**:
  - `runs`: `status`(succeeded/failed/cancelled/skipped/…)、`turn_no`、`started_at`/`finished_at`。
  - `events`(`src/events.rs`): `turn.nudged`・`validate.failed`・`turn.awaiting_human` などが run 単位で残る。
- **doctor** は `src/main.rs::cmd_doctor` +`doctor_agents`。`routing::GENERATED_AT`(= `"2026-07-12"`)は #64 で導入済み。既存の profile 検出は `run_capture(cmd, ["--version"])`。
- **CLI 検出のテスト注入**の前例: `routing::detect_command` をクロージャ差し替えする(`routing.rs` / `routing_test.rs`)。プローブも同じ流儀にする。
- スキーマ移行は `src/store/migrations/` の連番(次は `0007_`)、`MIGRATIONS` 配列に登録すれば冪等適用(`store/mod.rs`)。
- ドリフト sweep を載せる先は scheduler の per-project poll(`scheduler.rs` の `reaper::sweep` / `auto_merger::sweep` と同じ場所)。

## 決定(詳細は ADR 0007)

- 検査は 2 層独立。層 1 = doctor の静的鮮度、層 2 = 実履歴の成果ドリフト。
- 集計軸 = `runs.loop_kind` × `runs.agent_profile`。`impl-reviewer`(worker 内の 1 ターン、独立 run でない)は層 2 の対象外 = v1 の既知の限界として明記。
- 成功率の分母 = `succeeded`+`failed`+`cancelled` のみ(`skipped`/`needs_plan`/`decomposed` は除外)。この定義をテストで固定。
- コスト代理 = ターン数 × 所要時間。トークン会計はスコープ外。
- 読み取り(stats/doctor/top)は sqlite 直読み。ドリフト**検知**だけ scheduler の sweep が担う。sweep は (project, 役割, プロファイル) 単位の**現在の drift 状態**を `routing_drift` テーブルに UPSERT し(現状態のソースオブトゥルース)、状態が遷移したときだけ `routing.drift` / `routing.drift_cleared` イベントを履歴として追記する。doctor/top/stats は `routing_drift` を `project_id` で絞って未解消(`active`)行を読むだけ。`events` は `project_id` を持たず drift は run 非依存の集計なので、read 側のスコープ・解消判定はイベントではなく状態テーブルで担保する。
- **表示 3 コマンドの project スコープは既存 UI の流儀に合わせる**(コマンドごとに違う)。「現在 project」という概念は現行コードに存在しないので導入しない。代わりに (a) 全 project を **project 列付き**で出すか、(b) `--project <id>` で単一 project に絞るか、を各コマンドで確定する:
  - **`meguri stats routing`**: 既定は全 project を project 列付きで表示。`--project <id>`(`cmd_run`/`cmd_prune` と同じ optional 引数、`src/main.rs:34,37,53,56`)で単一 project に絞る。集計 `routing_stats` と drift 読みは引数があれば `project_id=?` で絞り、無ければ全 project。
  - **`meguri doctor`**: 現行 doctor は `cfg.projects` を全件ループする all-project コマンド(`src/main.rs:178`)。drift 検査も同様に全 project の未解消行を **project id を前置して**列挙する。project 選択フラグは足さない(doctor に単一 project という概念がないため)。
  - **`meguri top`**: 現行コードで明示的に cross-project view(`src/app.rs:573-581`, `src/app.rs:614-623`)。ヘッダの drift 行は全 project の active drift を**横断集計**する(合計件数 + project ごとの要約)。cross-project のまま。
  - どの表示でも drift 行は `routing_drift` を `WHERE project_id=?`(または全件+project 列)で読み、常に自身の project ラベルを伴う。よって「ある project の drift が別 project のものとして出る/混ざる」ことはない(all-project ビューでも project 列で分離される)。
- 閾値は**トップレベル** `[drift]` セクション(`[routing.drift]` にはしない)。既定 = 成功率 -20pt または平均ターン数 +50%。テストで固定。`[routing]` は書けば role routing が発動する switch(`Config.routing: Option<RoutingConfig>`、`src/config.rs:78` / `src/routing.rs:180`、ADR 0003)なので、TOML で `[routing]` テーブルを暗黙生成する `[routing.drift]` に閾値を置くと、legacy のまま drift だけ締めたいユーザーが意図せず `mode = auto` を発動させてしまう。drift 検知は routing の active/legacy と独立(legacy でも全 run は `default` プロファイルなので (役割, default) 単位で成績悪化を検出できる)なので、設定も routing から切り離す。
- プローブは「モデル不正 → ❌」「ネットワーク/認証失敗 → ⚠️」を区別。注入クロージャ + fake agent でテスト。

## 実装内容

### 層 1 — `meguri doctor` の拡張(`src/main.rs`)

1. **推奨表の賞味期限**: `routing::GENERATED_AT` を今日と比較し 90 日超で ⚠️「routing 推奨は YYYY-MM 版。新モデルのリリースを確認してください」。日付差の算出は `store::parse_ts`/`now` を使う純関数を `routing.rs` に足す(例 `routing::table_age_days()` → テスト可能)。閾値 90 日は定数。
2. **実起動プローブ**(opt-in、quota 消費): profile ごとに超短命 1 ターンを打ち、モデルエイリアスの生死を確認。結果を **model-invalid(❌)/ network・auth 失敗(⚠️)/ ok(✅)** に 3 分類。プローブ関数は `Fn(&AgentProfile) -> ProbeOutcome` のクロージャとして注入し、本番実装は該当 CLI を spawn(claude 系は `-p "reply: ok" --model <alias>` 相当)。既定でプローブを走らせるか `--probe` フラグにするかは spec レビューで決める(既定案: フラグ opt-in にして無課金の doctor を保つ)。
3. **CLI バージョンドリフト**: 検出した各 CLI の version 文字列を sqlite に UPSERT し、前回保存値とメジャー番号を比較。上がっていたら「挙動が変わっている可能性。ルーティング再評価を推奨」。メジャー抽出は先頭 `\d+` を拾う純関数(`vX.Y.Z` / `X.Y.Z` 双方)+ テスト。

### 層 2 — 成果集計とドリフト検知

4. **集計(sqlite 直読み)**: `Store` に `routing_stats(project: Option<&str>, window)` を追加(`None` = 全 project を project 列付きで、`Some(id)` = 単一 project)。`runs` は `project_id` を持つ(`0001_init.sql`、index `runs(project_id, loop_kind, issue_number)`)。(loop_kind, agent_profile) ごとに、直近 N 件の terminal run から成功率・平均ターン数(`AVG(turn_no)`)・平均所要時間(`finished_at - started_at` を `parse_ts` で秒化)を返す構造体 `RoutingStatRow` を返す。`agent_profile IS NULL`(未固定の旧 run)は「(unrouted)」等でまとめる。
5. **`meguri stats routing`**(`src/cli.rs` に `Stats { StatsCommand::Routing { project: Option<String> } }`、`src/main.rs` で dispatch、レンダリングは `src/app.rs`): 上記 3 指標を (役割, プロファイル) 表で表示。直近 drift があれば併記。**`--project <id>` 省略時は全 project を project 列付きで、指定時はその project だけ**を表示する(`routing_stats` と drift 読みへ `Option<&str>` project フィルタを渡す)。
6. **ドリフト検知(scheduler の sweep)**: 新モジュール(例 `src/engine/routing_drift.rs`)の `sweep(deps)` を poll に追加。project ごとに (役割, プロファイル) 単位で直近ウィンドウ(既定 20 run)と前ウィンドウを比較し、`成功率 -Δpt` か `平均ターン数 +Δ%` が閾値超えなら drift ありと判定。判定結果を `routing_drift` に UPSERT する(`active=1` + before/after 指標、または `active=0`)。**`active` が遷移したときだけ**イベントを追記する(0→1 で `routing.drift`、1→0 で `routing.drift_cleared`。payload に `project_id`/`role`/`profile`/`before`/`after`)。この「テーブルの現在値と比較して遷移時のみ書く」が dedup(同じ状態を毎 tick 書かない)の実装であり、テストで「同一状態の連続 sweep でイベントが 1 度だけ / 回復で cleared が 1 度だけ」を固定する。ウィンドウが埋まっていない (役割, プロファイル) は判定せず状態を書かない。
7. **表示**(project スコープは上記「決定」の通りコマンドごとに異なる):
   - **doctor**: 全 project(`cfg.projects` ループ)の `routing_drift` 未解消(`active`)行を **project id を前置して**「[proj] worker/claude-sonnet の成績が悪化 — CLI 更新かモデル変更の影響の可能性」を ⚠️ 表示。回復済み(`active=0`)は出さない。
   - **`meguri top`**(`src/app.rs::render_top` / `TopStatus`): cross-project のまま、全 project の active drift を横断集計してヘッダに drift 件数/要約行を 1 行追加。
   - **`meguri stats routing`**: 上記 5 の通り、`--project` 無しは全 project(project 列付き)、有りは単一 project。

### 設定(`src/config.rs`)

8. **`Config` に**トップレベル `#[serde(default)] drift: DriftConfig`(TOML `[drift]`)を追加する。`RoutingConfig` の中には**入れない** — `Config.routing` は `Option<RoutingConfig>` で `[routing]` の存在そのものが role routing の switch(`src/config.rs:78`, `src/routing.rs:180`, ADR 0003)なので、TOML で `[routing]` を暗黙生成する `[routing.drift]` に閾値を置くと legacy ユーザーが意図せず routing を active 化してしまう。`DriftConfig { success_rate_drop_pt: f64 = 20.0, turns_increase_pct: f64 = 50.0, window: usize = 20 }`(既定値関数 + `Default`)。トップレベルかつ `#[serde(default)]` なので `[drift]` 無し(既定運用)でも `[routing]` 無し(legacy)でも既定が引け、どちらも routing の active 判定に影響しないことをテストで固定する(`[drift]` だけの config で `cfg.routing` が `None` のまま)。

### スキーマ(`src/store/migrations/0007_routing_freshness.sql`)

9. **CLI バージョン履歴**の最小永続化。`CREATE TABLE cli_versions (command TEXT PRIMARY KEY, version TEXT NOT NULL, major INTEGER, checked_at TEXT NOT NULL)` を UPSERT。
10. **drift 現在状態テーブル**。read 側のスコープ(project 別)と解消判定を担保するため、集計イベントとは別に現在状態を 1 行 = 1 (project, 役割, プロファイル) で持つ:
    ```sql
    CREATE TABLE routing_drift (
      project_id     TEXT NOT NULL,
      loop_kind      TEXT NOT NULL,
      agent_profile  TEXT NOT NULL DEFAULT '',   -- '' = unrouted(NULL 不可で複合 PK を成立させる)
      active         INTEGER NOT NULL,           -- 1 = 未解消 / 0 = 解消
      metric_json    TEXT NOT NULL DEFAULT '{}', -- before/after 指標
      detected_at    TEXT NOT NULL,              -- drift 開始時刻(active=1 になった時)
      updated_at     TEXT NOT NULL,
      PRIMARY KEY (project_id, loop_kind, agent_profile)
    );
    ```
    sweep がここへ UPSERT し、doctor/top/stats は `WHERE project_id=? AND active=1` で読む。`routing.drift` / `routing.drift_cleared` イベントは履歴(閾値を跨いだ瞬間の記録)として `events` に残すが、read 側の現在状態はこのテーブルが真。

両テーブルとも `MIGRATIONS` 配列に登録(冪等適用)。

## 受け入れ条件(元 issue から）

- [ ] `meguri stats routing` が (役割, プロファイル) 別の成功率・平均ターン数・平均所要時間を表示する
- [ ] ウィンドウ比較でドリフトが `routing_drift` に記録され(`routing.drift` イベントも追記)、`meguri top` と doctor に表示される(閾値はテストで固定 / serve 撤去に伴い web ページではなく top+stats に表示)
- [ ] 複数 project の drift が `project_id` で正しく分離される: `stats routing --project X` は X の drift だけを出す / doctor・top・`stats routing`(全 project 表示)では各 drift 行が自分の project ラベルを伴い、別 project のものとして混同されない
- [ ] 成績が閾値内に回復すると `active=0` になり(`routing.drift_cleared` イベント)、doctor/top/stats から消える。同一状態の連続 sweep でイベントが増えない(テストで固定)
- [ ] doctor: `GENERATED_AT` 90 日超で ⚠️ が出る
- [ ] doctor: 実起動プローブが無効モデルを ❌ で検知する(fake agent = 注入クロージャでのテスト)。ネットワーク/認証失敗は ⚠️ に落ちて doctor を fail させない
- [ ] doctor: CLI メジャーバージョン変化で再評価推奨が出る(sqlite 前回値との比較をテストで固定)

## 触るファイル

- `src/main.rs` — doctor に 3 検査(表鮮度 / プローブ / CLI バージョン)を追加
- `src/routing.rs` — `table_age_days()` 等の純関数、プローブ結果型 `ProbeOutcome`、プローブ本番実装
- `src/cli.rs` — `Stats` サブコマンド(`routing { project: Option<String> }`)
- `src/app.rs` — `meguri stats routing` レンダリング(全 project = project 列付き / `--project` = 単一)、`render_top`/`TopStatus` に cross-project drift 行
- `src/engine/routing_drift.rs`(新規)+ `src/engine/scheduler.rs` — poll に drift sweep を追加、`super::routing_drift` を配線
- `src/store/runs.rs`(または新 `src/store/stats.rs`) — `routing_stats(project: Option<&str>, window)`、CLI バージョン UPSERT/読み出し、`routing_drift` の UPSERT / project 別(または全 project)未解消行の読み出し
- `src/store/migrations/0007_routing_freshness.sql` + `src/store/mod.rs` — `cli_versions` + `routing_drift` の移行登録
- `src/config.rs` — トップレベル `[drift]`(`DriftConfig`)。`RoutingConfig` の外に置き routing の activation に影響させない
- `docs/adr/0007-routing-freshness-and-outcome-drift.md` — 本 PR 同梱
- tests: `tests/stats_routing_test.rs`(集計 + ドリフト閾値)、doctor プローブ/バージョンのユニットテスト(`main.rs`/`routing.rs`/`store`)

## スコープ外

- Claude Code セッション jsonl からのトークン usage 収集(将来拡張、issue 明記)。
- `impl-reviewer` のターン単位成果集計(独立 run でないため層 2 対象外。層 1 のプローブ検証は受ける)。
- routing 3/3(ドリフト検知を受けたエスカレーション/自動再評価)。
- Web ダッシュボード復活(serve は撤去済み。表示は top + stats に集約)。
