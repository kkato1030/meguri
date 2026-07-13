# issue-146 spec — 時刻駆動運用 (1/2): `[[projects.schedules]]` cron 起票

この spec の決定は一行で書ける。**`meguri watch` の poll tick に cron スケジュール評価を載せ、テンプレートから issue(github mode)またはローカルタスク(local mode)を定期起票する帯域外 sweep を新設する。** 発火は「キューに1件積む」だけで、消化は既存の worker / planner ループがそのまま担う(境界は同梱の ADR 0009)。

## 構造の決定: Loop ではなく「帯域外 sweep」

これがこの spec の一番大きい構造判断。発火がやるのは `forge.create_issue(...)`(github)/ `store.create_task(...)`(local)を1回呼ぶことだけで、agent pane も run レコードも要らない。よって既存の `Loop` trait(discover/drive で pane を持つ run を生む)ではなく、`reaper::sweep` / `auto_merger::sweep` / `merge_watch::sweep` と同じ**帯域外 sweep**として実装する(`src/engine/scheduler.rs:96-112` の per-project ブロックに1つ足す)。

- 新モジュール `src/engine/scheduler_fire.rs`(名前は仮。`schedule.rs` でも可)に `pub async fn sweep(deps: &Deps, now: u64) -> Result<()>` を置く。
- 呼び出しは既存の per-project sweep ブロック内。発火した issue/タスクが同 tick で拾われるよう **discovery より前**に呼ぶのが望ましいが、poll_interval(≈60s)粒度なので1 tick 遅れても実害はない。実装容易さを優先して既存 sweep ブロック(discovery 後)に相乗りしてよい。

## 時刻ソースは注入する(秒精度不要)

現状 clock 抽象は無い(chrono も無い)。cleaner の `needs_scan(marker, head, now, interval)` と同じく、**時刻を引数で渡す純粋関数**にして注入可能にする(trait は増やさない)。sweep 呼び出し側(scheduler)が `now: u64`(epoch 秒、`cleaner::epoch_now()` 同型)を渡し、テストは任意の値を注入する。判定粒度は poll_interval で十分(issue の指定どおり)。永続タイムスタンプは既存規約に合わせ `store::now()`(RFC3339 UTC)/ `store::parse_ts()` を再利用する。

## 最終発火時刻を sqlite に永続化 + catch-up 折りたたみ

config hot reload(#73)でスケジュール定義は watch 再起動なしに増減するので、最終発火時刻は **config でなく sqlite** に置く(issue の指定どおり)。新テーブル(migration `0008_schedules.sql`、`MIGRATIONS` に1行追記、`src/store/schedules.rs` に CRUD — いずれも `store/tasks.rs` と `0002_heartbeats.sql` を雛形にする):

```sql
CREATE TABLE IF NOT EXISTS schedule_state (
  project_id    TEXT NOT NULL,
  name          TEXT NOT NULL,          -- schedule 名(project 内で一意)
  first_seen_at TEXT NOT NULL,          -- 初観測時刻。backfill 防止の窓の下端
  last_fired_at TEXT,                   -- 最終発火。NULL=未発火
  last_key      INTEGER,                -- 直近作成した issue 番号(github)/ task id(local)
  PRIMARY KEY (project_id, name)
);
```

発火判定(純粋関数、`cleaner::needs_scan` と同じ流儀):

1. その schedule の行が無ければ **今回は発火せず** `first_seen_at = now` で seed する。これで新規追加スケジュール(初回起動時 / hot reload での追加)が過去分を一気に起票する事故を防ぐ。
2. 窓の下端 = `max(last_fired_at, first_seen_at)`。cron 式がこの下端 `now` の間に1回以上ヒットすれば発火する。
3. **停止をまたいだ未発火分は catch-up せず1回に折りたたむ**(cron デーモンの一般則)。窓内に cron 発火時刻が何個あろうと起票は1件。発火したら `last_fired_at = now` に更新する。

## cron 評価(要レビュー判断)

標準5フィールド(分・時・日・月・曜日)。必要な原始操作は「時刻 `t` の次の cron 発火時刻」= 発火判定(窓内ヒットの有無)と `meguri schedules` の next-fire 表示に使う。

**推奨: 依存を足さず自前実装する。** このリポジトリは chrono を持たず日付演算を自前化している(`store::now()` の Howard-Hinnant civil-date アルゴリズム)。標準5フィールド + 分粒度なら cron パーサ/評価器は小さく収まり、既存の civil-date ヘルパ(と weekday 導出の追加)で書ける。`*` / 範囲 `a-b` / ステップ `*/n`, `a-b/n` / リスト `a,b` に対応すれば足りる。`src/cron.rs` に純粋関数として置き、単体テストで網羅する。
- **代替**(レビューで自前実装を嫌う場合): `saffron` などの cron crate を採用する(ただし chrono を引き込む。リポジトリの無依存志向とのトレードオフ)。

## タイムゾーン(要レビュー判断)

**推奨: v1 は UTC 固定。** `store::now()` が UTC であること・注入クロックでの決定的テスト・DST/オフセットの複雑さ回避を優先する。cron フィールドは UTC 解釈であることを README / doctor / `meguri schedules` に明記する。ローカル時刻で回したい運用者は cron 側でオフセットする。per-schedule の `timezone` 指定は将来 issue に切り出す(スコープ外)。

## 起票

- **github mode**: `forge.create_issue(title, body, &[label])`。label は `kind` から引く — `kind = "ready"` → `LABEL_READY`(worker 行き)、`kind = "plan"` → `LABEL_PLAN`(planner 行き)。ラベル二軸(ADR 0005)のフェーズ軸を1枚だけ付ける。作成した issue 番号を `schedule_state.last_key` に記録。
- **local mode**: `store.create_task(project_id, kind, title, body, origin)`。`kind = "ready"` → task kind `"work"`、`"plan"` → `"plan"`。`origin` は `schedule:<name>`(既存の `local` / `github:<N>` に倣った新しい origin)。作成した task id を `last_key` に記録。
- **`title` テンプレート変数は最小限**: 発火日付 `{{date}}`(`YYYY-MM-DD`, UTC)だけから始める。本文の動的生成は入れない(ADR 0009: 欲しくなったら #120 と組み合わせて解決)。

## 重複ガード(要レビュー判断)

同じ schedule 起源の open issue/task が残っていたら既定でスキップ。`allow_overlap = true` で無効化。

- **識別子**: 作成物には人間可読の provenance として hidden マーカー `<!-- meguri:schedule name=<name> -->` を本文に埋める(cleaner の head-sha マーカーと同じ流儀)。local task は加えて `origin = schedule:<name>` を持つ。
- **openness 判定(推奨)**: 重複判定そのものは `schedule_state.last_key` を引き、その issue/task が open かを直接見る — github は `forge.issue_state(last_key)`、local は task の status。GitHub 全文検索を新設せず、決定的で安い。直近発火物の openness だけを見る(直列に発火するので通常これで十分)。
  - **代替**(Authority 純度を優先し「マーカーが唯一の識別」にこだわる場合): `forge` にマーカー本文検索(`gh search issues ... in:body`)を足して open issue を引く。`last_key` に頼らない反面 API コストと索引依存が増える。

## `meguri doctor` の検証

- **hard(load 時、`Config::validate()` `src/config.rs:679-702`)**: cron 式がパースできること、`name` が project 内で一意なこと、`body_file` と `body` が排他かつどちらか一方あること。`watch` の起動/hot reload を壊す不正はここで弾く(`bail!`)。
- **soft(`meguri doctor`、`src/main.rs`)**: `doctor_schedules(cfg) -> bool` を `check_auto_merge` と同じ雛形で追加し、`body_file`(repo 相対)の実在を検証・各 schedule の次回発火時刻を人間可読に表示する。`cmd_doctor` の集計 `ok &= ...` に配線。

## 一覧性: `meguri schedules`

専用サブコマンド `meguri schedules`(nested subcommand は不要、`Doctor` 型の単純変種)を追加し、定義・最終発火・次回発火を表で出す。`ps`/`top` は run 中心なので schedule は別コマンドが自然。`src/cli.rs` に variant 追加 → `src/main.rs` で `app::cmd_schedules` に委譲。

## config スキーマ

`ProjectConfig`(`src/config.rs:610`)に `#[serde(default)] pub schedules: Vec<ScheduleConfig>` を追加(`Vec<ProjectConfig>` が既に nested `[[...]]` を証明済み)。`ScheduleConfig` は `WorktreeSetupConfig` を雛形に:

```toml
[[projects.schedules]]
name = "daily-tidy"              # project 内で一意
cron = "0 9 * * *"              # 標準5フィールド、UTC 解釈
kind = "ready"                  # "ready"(worker)| "plan"(planner)
title = "Daily tidy {{date}}"  # テンプレート、変数は {{date}} のみ
body_file = "ops/daily-tidy.md" # repo 相対。または body(排他):
# body = "インライン本文"
# allow_overlap = false         # 既定 false: 起源の open issue/task が残っていればスキップ
```

`kind` は lowercase-rename enum(`ProjectMode` 等と同じ idiom)。`body_file` / `body` は排他 `Option`。

## 触るファイル

- `src/config.rs` — `ScheduleConfig` + `ProjectConfig.schedules`、`validate()` に cron/一意性/本文排他の hard 検証
- `src/store/migrations/0008_schedules.sql`(新規)+ `src/store/mod.rs` の `MIGRATIONS` に1行 + `src/store/schedules.rs`(新規、`schedule_state` の CRUD)
- `src/cron.rs`(新規、自前採用時) — 5フィールドのパース + 発火判定 + next-fire、純粋関数
- `src/engine/scheduler_fire.rs`(新規) — `sweep(deps, now)`: 発火判定 → 起票 → state 更新 → 重複ガード
- `src/engine/scheduler.rs` — per-project sweep ブロックに `scheduler_fire::sweep` を配線、`now` を渡す
- `src/tasks.rs` / `src/store/tasks.rs` — local の重複ガード用に `schedule:<name>` origin での open task 照会(必要なら)
- `src/cli.rs` / `src/main.rs` / `src/app.rs` — `meguri schedules` コマンド
- `src/main.rs` — `doctor_schedules` の soft 検証
- `README.md` / `README.ja.md` — `[[projects.schedules]]` の説明(cron は UTC、起票のみ、重複ガード、`allow_overlap`)
- `docs/architecture/loops.md` — §2 帯域外 sweep 表に schedule sweep を追記
- `tests/schedule_test.rs`(新規)
- `docs/adr/0009-schedules-enqueue-only-not-a-cron-replacement.md`(本 PR 同梱)

## 受け入れ基準

1. `[[projects.schedules]]` を config に書くと watch が読み、cron がヒットした tick で github mode は `kind` 相当のフェーズラベル付き issue を、local mode は対応 kind のタスクを1件作る。
2. 作成物は既存の worker / planner discovery にそのまま拾われる(FakeForge / local store 上で実証)。
3. 最終発火時刻が sqlite(`schedule_state`)に永続化される。プロセスを止めて cron 発火時刻を複数またいで再開しても、起票は **1件に折りたたまれる**(catch-up しない)。
4. 新規追加スケジュール(初回観測)は過去分を backfill せず、次回発火から起票する。
5. 重複ガード: 同一 schedule 起源の open issue/task が残っていれば既定でスキップ。`allow_overlap = true` で毎回起票する。
6. hot reload(#73)でスケジュール定義を足すと、watch 再起動なしに次 tick 以降で有効になる(最終発火は sqlite にあるため定義側の変更で失われない)。
7. `meguri doctor` が不正な cron 式・存在しない `body_file`・重複 `name`・`body`/`body_file` の同時指定を報告する。不正 cron / 本文欠落は `watch` 起動時にも `Config::validate()` で弾かれる。
8. `meguri schedules` が定義・最終発火・次回発火を表示する。
9. 時刻はテストで注入でき(注入クロック)、発火・折りたたみ・重複ガード・hot reload での定義追加が fake forge / fake clock で検証される。
10. 既存テストが全て通る(特に `scheduler_test.rs` の非破壊、`store` の migration 冪等性テスト)。

## テスト計画

`tests/schedule_test.rs` を新設。cron 評価は `src/cron.rs` の純粋関数を単体で網羅(`*` / 範囲 / ステップ / リスト / 曜日、窓内ヒット判定、next-fire)。sweep はメモリ store + FakeForge に固定 `now` を渡して駆動し、受け入れ基準 1〜6 を検証する — 特に (3) 折りたたみ(下端と `now` の間に複数発火時刻を置いて起票1件を確認)、(4) backfill 抑止(first_seen 直後に過去 cron 時刻があっても発火しない)、(5) 重複ガード(`last_key` が open の間はスキップ、close 後に再発火)、(6) hot reload での定義追加。migration は既存の冪等性テスト(`store/mod.rs`)に `schedule_state` を通す。

## スコープ外(将来)

- 任意コマンドの定期実行(cron 置き換え)— ADR 0009 で恒久的に線引き。
- 本文の動的/AI 生成 — #120(capture-first)との組み合わせで解決。
- per-schedule の `timezone` 指定(v1 は UTC 固定)。
- discovery の cadence 制御(not-before / 消化レート上限)— 後続 #148(2/2)。
