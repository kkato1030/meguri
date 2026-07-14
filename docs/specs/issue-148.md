# issue-148 spec — 時刻駆動運用 (2/2): discovery の cadence 制御(not-before / 消化レート上限)

discovery はキューにある actionable なタスクを即座に消化するため、**時刻に縛られたタスクをキュー駆動で表現できない**(公開解禁日待ち、「1日1本」のレート制約)。discover(`src/tasks.rs`)の同じ層に、claim より前・dependencies チェックと同格の2つの調速ゲート — **not-before**(この日時までは discover しない)と **cadence**(この期間の消化実績が上限なら discover しない)— を足す。ゲートに引っかかったタスクは forge 側に痕跡を残さずサイレントにスキップし、可視化はローカル CLI(`meguri tasks`)が担う。

設計判断の根拠(なぜこの流儀か・消化実績をローカル run 履歴で数える理由・fail-closed・窓と TZ の割り切り)は本 PR 同梱の **ADR 0011** に置く。本 spec は「何を・どこを触り・受け入れ基準は何か」に絞る。

## 決定サマリ(ADR 0011 の要約)

- **場所**: not-before → dependencies → cadence の順で、`has_unresolved_blockers` と同じ discover 内の層に挿す(claim より前)。**cadence は最後**に置き、共有残枠は他ゲート(特に dependencies)を通過した actionable な候補にだけ配る(blocked 候補が残枠を食って後続を締め出す事故を避ける。ADR 0011 帰結)。スキップはラベルもコメントも書かない(dependencies ブロックと同じ流儀)。
- **not-before 表現**: github は本文 hidden マーカー `<!-- meguri:not-before <TS> -->`(cleaner / #146 と同じ流儀。ラベルは採らない)。local は `tasks.not_before` フィールド + `meguri add --not-before <TS>`。
- **cadence 表現**: config `[[projects.cadence]]`(array-of-tables、schedules と同じ並び)で `label` → `max_per_day` または (`per_hours` + `max`)。**github(issue ラベル)専用**(local タスクにラベル軸が無いため v1 スコープ外)。
- **消化カウント**: forge に持たず、ローカル `runs` から数える。`runs.cadence_label` は run 作成の **`INSERT` と同一文**で刻み(後追い UPDATE の2手順にしない)、窓内の `skipped` でない run 数を消化数とする(成否によらず1消化)。窓は `max_per_day`=UTC 暦日 / `per_hours`=ローリング。
- **1 issue 1 バケツ(v1)**: cadence rule に一致するラベルは issue あたり最大1つ。2つ以上一致する issue は fail-closed(discover しない)とし、`meguri tasks` が理由付きで見せる。
- **可視化**: discover と同じゲート関数を読み取り専用で回し、`meguri tasks` が「not-before 待ち」「cadence 待ち」を理由付きで表示する。

## 触るファイル

### 新規

- **`src/cadence.rs`**(新モジュール) — 時刻ゲートのロジックを1か所に集約:
  - `parse_not_before(body: &str) -> Result<Option<u64>, ParseErr>`: 本文から `<!-- meguri:not-before <TS> -->` を抽出。`<TS>` は `YYYY-MM-DD`(→ `T00:00:00Z` 補完)または RFC3339 `...Z`。`store::parse_ts` を再利用。複数マーカーは最も遅い(最も制約が強い)ものを採用。解析不能は `Err`(呼び出し側が fail-closed 判定に使う)。
  - `not_before_wait(not_before: Option<u64>, now: u64) -> Option<u64>`: 未通過なら通過予定時刻を返す。
  - `CadenceRule` の窓計算 `window_start(rule, now) -> u64`(`max_per_day`=UTC 暦日 00:00 / `per_hours`=`now - H*3600`)。UTC 暦日は `cron::civil_from_epoch` を再利用して算出。
  - `cadence_bucket(issue_labels: &[String], rules: &[CadenceRule]) -> Result<Option<String>, ConflictingLabels>`: issue のラベルを rule と突き合わせ、一致 0 なら `None`、ちょうど 1 なら `Some(label)`、2 以上は `Err`(fail-closed 判定用。一致した全ラベルを持つ)。discover と `cmd_run` と `cmd_tasks` がこの単一実装を共有する。
  - `Disposition` enum: `Ready` / `WaitingNotBefore { until }` / `WaitingCadence { label, consumed, max, resets_at }` / `UnparsableNotBefore` / `ConflictingCadenceLabels { labels }`。discover はこれで絞り、CLI はこれを表示。
- **`src/store/migrations/0011_cadence.sql`** — `ALTER TABLE runs ADD COLUMN cadence_label TEXT`(NULL 可)、`ALTER TABLE tasks ADD COLUMN not_before TEXT`(NULL 可、RFC3339 UTC)。migration 0007 で runs は再作成済みなので単純 ALTER で可。窓内カウント用に `CREATE INDEX idx_runs_cadence ON runs(project_id, cadence_label, created_at)`。

### 変更

- **`src/config.rs`**:
  - `CadenceRule { label: String, max_per_day: Option<u32>, per_hours: Option<u32>, max: Option<u32> }` と `ProjectConfig.cadence: Vec<CadenceRule>`(`#[serde(default)]`)。
  - `validate_cadence(p)`(schedules と並べる): `label` 非空・プロジェクト内一意、期間モードは `max_per_day` 単独 **xor** (`per_hours` + `max`) のちょうど一方、数値 > 0。違反は config load で bail。
- **`src/tasks.rs`**:
  - `LabelTaskSource` / `LocalTaskSource` に注入可能な epoch clock を保持(既定=システム、テストは fake)。`Deps::with_label_source` と `app::build_coordination`、`LocalTaskSource::new` の呼び出し側を既定 clock で更新。
  - `LabelTaskSource::discover`: not-before(本文マーカー)ゲートを `has_unresolved_blockers` の**直前**に、cadence(`self.cadence_rules` × `store` の窓内カウント)ゲートを**直後**(dependencies 通過後)に追加。バケツ判定は `cadence::cadence_bucket` — 一致 1 ラベルなら cadence 通過時に `Task` に cadence バケツを載せ、**2 ラベル以上一致は fail-closed でサイレントスキップ**(単一バケツ設計では片方しか刻めず、もう片方の上限が破れるため。可視化は `meguri tasks` の `ConflictingCadenceLabels`)。cadence rule は `project.cadence` を source が保持。
  - `LocalTaskSource::discover`: `not_before` フィールドで not-before ゲート(cadence は github 専用なので無し)。
  - 消化カウント: 下記 `Store::cadence_consumed` を使い、`残枠 = max - consumed` を FIFO で配る(残枠超過分は返さない)。**残枠を配る対象は dependencies を通過した actionable な候補のみ** — blocked 候補は cadence ゲートに到達する前に外れているので残枠を消費しない。
- **`src/engine/mod.rs`**: `Target` に `cadence_label: Option<String>` を追加。`worker.rs` / `planner.rs` の discover が `Task.cadence_label` を `Target` へ透過。
- **`src/engine/scheduler.rs`**: `TaskKey::Issue` 分岐の run 作成で `target.cadence_label` をそのまま `create_run_for_loop` に渡す(`TaskKey::Local` 分岐は `create_run_for_task` のままで cadence 対象外)。刻印は run 作成の `INSERT` に含まれる(下記 `runs.rs`)ため、scheduler 側に後追いの刻印手順は無い。
- **`src/store/runs.rs`**:
  - `create_run_for_loop` に `cadence_label: Option<&str>` を追加し、**run 作成の `INSERT` と同一文で `cadence_label` を書く**(後追い `UPDATE` の2手順にしない)。run とバケツ刻印が1文で確定するので、作成直後にプロセスが落ちても「run は残るが `cadence_label` が NULL」という窓内 COUNT 漏れ(→ 次 tick が同バケツをもう1件流す上限超過)は構造的に起きない(ADR 0011「バケツは run 作成時に確定」の実装)。`create_run` ラッパーにも同引数を通し、既存呼び出し側は `None` を渡す。`create_run_for_task` は local = cadence 対象外なので変更しない(NULL のまま)。
  - `cadence_consumed(project_id, label, window_start: u64) -> i64`: `SELECT COUNT(*) FROM runs WHERE project_id=? AND cadence_label=? AND created_at >= ? AND status != 'skipped'`。**`runs.created_at` は `now()` が入れる RFC3339 UTC 文字列(`YYYY-MM-DDThh:mm:ssZ`、`src/store/runs.rs`)なので、epoch の `window_start` はそのまま渡さず `store::format_epoch(window_start)` で同じ RFC3339 文字列に変換してから `created_at >= ?` に束縛する**(この shape は辞書順=時刻順なので TEXT 比較で正しく窓内 COUNT できる)。epoch 整数を直接渡すと SQLite の型優先順位で文字列比較になり窓外の run まで数えてしまう。`cadence.rs` 側の `window_start(rule, now) -> u64` は epoch のまま返し、store 境界で文字列化する。
  - `run_from_row` / `RunRecord` に `cadence_label` を反映。
- **`src/store/tasks.rs`**: `create_task` に `not_before: Option<&str>` を通す。`TaskRow` に `not_before` を反映。
- **`src/app.rs` (`cmd_add`)**: `--not-before <TS>` を受け、RFC3339/日付を正規化して保存。
- **`src/app.rs` (`cmd_run`)**: 手動 run は discover / scheduler を通らず `deps.store.create_run(...)` を直接叩くため(`src/app.rs`)、cadence を刻む経路がここには無い。`create_run` の**前**に、fetch 済みの `gh_issue.labels`(`forge::Issue.labels`)を `cadence::cadence_bucket` で `project.cadence` の rule と突き合わせ、一致する `label` を `create_run` の引数として渡す — 刻印は run 作成の `INSERT` に含まれる(ゲートはバイパスし常に実行、消化には数える。ADR 0011)。既存 run の resume 経路(`create_run` が `Err` → 既存 run を拾う分岐)は新規 `INSERT` をしないので、元の刻印がそのまま残り重複計上は起きない。一致 rule が無ければ従来どおり NULL。**2 rule 以上に一致する issue は bail**(どちらのバケツで数えるべきか人間にも決められないため。エラーメッセージでラベルの整理を促す)。
- **`src/cli.rs`**: `Add` に `--not-before <String>`。
- **`src/app.rs` (`cmd_tasks`)** — 可視化(下記「可視化」節):
  - local mode: 既存のローカルタスク一覧に、`not_before` 由来の「⏳ not-before 待ち(until …)」注記を足す。
  - github mode: 現状ほぼ空表示なので、`ready`/`plan` ラベル issue を fetch し、各 issue の `Disposition` を表示(`Ready` / not-before 待ち / cadence 待ち / cadence ラベル競合 / blocked)。discover と同じ `cadence` 関数を読み取り専用で回す。→ `cmd_tasks` を async 化し、github mode は forge を引く。
- **`src/main.rs` (`doctor_cadence`)** — 新セクション: 各プロジェクトの cadence rule を列挙し、`label`・期間モード・現在の窓内消化数/上限/残枠を表示(store を引いて `cadence_consumed`)。config load 時点で shape 検証済みなので、doctor は「今どうなっているか」を見せる係。`doctor_schedules` の直後に並べる。
- **`docs/architecture/loops.md`**: discovery の入力条件に「not-before / cadence の2ゲート(サイレントスキップ、実績はローカル run 履歴)」を1段落追記。README(en/ja)にも `[[projects.cadence]]` と not-before マーカーの短い説明を足す。

## 主要な決定(レビューで詰めたい点)

1. **可視化の主面は `meguri tasks`、`meguri ps` は現状維持(要レビュー)。** サイレントスキップされたタスクは run を持たないため、run 一覧である `ps` には本質的に出ない。`ps` を offline(sqlite のみ、watch 停止中でも動く)に保つ性質を壊したくないので、github の待機可視化(forge fetch が要る)は「キュー点検コマンド」である `tasks` に寄せる。`ps` にも出したい場合の代替は「discover が待機理由を sqlite の観測テーブルに毎 tick 書き、両コマンドが offline 読みする」だが、テーブル増設と毎 tick 書き込みの重さに見合わないと判断。**この線引きはレビューで確認したい。**
2. **cadence の消化は成否によらず1消化(`skipped` のみ除外)。** 「1日1本」= 試行1回。失敗した投稿を無制限リトライで枠を食わせない(ADR 0011)。
3. **窓の TZ は v1 UTC。** #146 の UTC-only 前例に揃える。UTC 深夜ロールオーバーがずれる運用は `per_hours` で回避。設定可能オフセットは将来課題。
4. **not-before 解析不能は fail-closed。** 解禁日タイポで早期公開する事故を避ける。詰まりは `meguri tasks` の `UnparsableNotBefore` 表示で可視化。
5. **cadence は github(ラベル)専用。** local タスクにラベル軸が無いため v1 スコープ外。
6. **手動 `meguri run --issue` は cadence をバイパスするが消化に数える。** 人間の明示上書きなので残枠チェックで止めないが、issue のラベルが cadence 対象なら run に刻んで窓内カウントに含める(そうしないと同日 `watch` が同ラベルを追加消化して上限超過する)。ADR 0011 の不変条件に対応。
7. **cadence は dependencies の後(最後のゲート)。** 共有残枠は actionable な候補だけに配る。blocked 候補に残枠を先食いさせると後続の実行可能 issue が毎 tick 締め出される(meguri review 指摘)。
8. **cadence rule に一致するラベルは issue あたり最大1つ(v1)。** GitHub issue は複数ラベルを普通に持てるが、`runs.cadence_label` は単一バケツなので、複数 rule に一致した issue の run をどちらか一方だけに刻むともう一方の上限が守られない。v1 は複数一致を fail-closed(discover しない、`meguri run` は bail)とし、`meguri tasks` が `ConflictingCadenceLabels` として理由付きで見せる(meguri review 指摘)。複数バケツへの同時刻印(join table 化)は必要になったら別 issue。

## 受け入れ基準(acceptance criteria)

1. **not-before(github)**: 本文に `<!-- meguri:not-before <未来> -->` を持つ `ready` issue は discover に載らず、ラベル・コメントが一切書かれない。時刻通過後(fake clock)に載る。
2. **not-before(local)**: `meguri add --not-before <未来>` したタスクは discover されず、通過後に discover される。`meguri tasks` が通過前は「not-before 待ち(until …)」を表示する。
3. **not-before 解析不能**: 壊れたマーカー/フィールドは discover されず(fail-closed)、`meguri tasks` が `UnparsableNotBefore` として理由付きで見せる。
4. **cadence 上限到達**: `[[projects.cadence]] label="sns" max_per_day=1` で、当日 `sns` の run が1件立っていれば、別の `sns` issue は discover に載らない(ラベル・コメントなし)。上限未満なら載る。
5. **窓ロールオーバー**: fake clock で UTC 暦日(`max_per_day`)/ ローリング窓(`per_hours`)をまたぐと消化数がリセットされ、再び discover に載る。
6. **消化カウントの規則**: `skipped` run は消化に数えず、`failed`/`succeeded` は各1消化として数える(窓内 COUNT の対象)。cadence バケツは run 作成の `INSERT` と同一文で刻まれ(`cadence_label` が NULL の `sns` run が観測できる瞬間は存在しない)、以後 issue のラベルが変わっても過去実績は不変。
7. **複数ラベル併用**: 異なる cadence ラベル(例 `sns` と `newsletter`)は独立に窓を持ち、片方の上限到達がもう片方を止めない。
8. **config 検証 / doctor**: `label` 空 / 重複、期間モードの指定漏れ・両指定、非正の値は config load で拒否。`meguri doctor` の cadence セクションが各 rule の窓内消化数/上限/残枠を表示する。
9. **可視化**: `meguri tasks` が not-before 待ち・cadence 待ちを理由付きで表示する(github mode は forge を引いて `ready`/`plan` issue の disposition を出す)。
10. **非破壊**: 既存の `tasks` / `runs` / scheduler / config テストが全て通る。cadence 未設定・not-before 無しのプロジェクトは従来どおりの discover 挙動(追加のスキップが起きない)。
11. **cadence は dependencies の後**: `max_per_day=1` で、古い `sns` issue が未解決ブロッカー付き・後続の `sns` issue がブロッカー無しのとき、blocked な古い issue は残枠を消費せず、後続の actionable な issue が当日の1枠に載る(blocked 候補の先食いで actionable 候補が締め出されない)。
12. **手動 run の消化計上**: cadence 対象ラベルを持つ issue を `meguri run --issue` で実行すると、その run に `cadence_label` が刻まれ窓内カウントに数えられ、同じ窓で `watch` が同ラベルの別 issue を discover しない(上限超過しない)。cadence 未対象ラベルの手動 run は `cadence_label` が NULL のまま。
13. **cadence ラベル複数一致は fail-closed**: 2つ以上の cadence rule(例 `sns` と `newsletter`)に一致するラベルを持つ issue は discover に載らず(ラベル・コメントなし)、`meguri tasks` が「cadence ラベル競合(sns, newsletter)」として理由付きで表示し、`meguri run --issue` はエラーで止まる。片方のラベルを外せば通常どおり discover される。

## テスト計画

- **`src/cadence.rs` 単体**: マーカー抽出(日付/RFC3339/複数/不正)、`window_start`(UTC 暦日境界・ローリング)、`not_before_wait`、`cadence_bucket`(一致 0/1/2+ の3ケース)。
- **`tests/`(新規 `cadence_test.rs`)**: FakeForge + fake clock で —
  - not-before 通過(github マーカー / local フィールド)。
  - レート窓のロールオーバー(`max_per_day` の UTC 暦日跨ぎ、`per_hours` のローリング跨ぎ)。
  - 複数ラベル併用の独立性。
  - `skipped` run が消化に数えられないこと。
  - サイレント性(スキップ時に FakeForge へラベル/コメントが増えないこと)。
  - blocked 候補が残枠を先食いしないこと(blocker 付き古 issue + blocker 無し後続で、後続が枠に載る)。
  - 手動 run(cadence 対象ラベル)が窓内カウントに数えられ、同窓の `watch` を止めること。
  - 複数 cadence rule に一致する issue が discover されないこと(fail-closed、FakeForge に痕跡なし)。
- **`src/tasks.rs` 内 tests**: 注入 clock 付きで discover のゲート順序(not-before → dependencies → cadence)と cadence の残枠配布(actionable 候補のみ FIFO)。
- **config tests**: `[[projects.cadence]]` のパースと `validate_cadence` の各拒否ケース。

## スコープ外(将来 / 別 issue)

- local mode の cadence(ラベル軸を local タスクに導入する話。v1 は github 専用)。
- 複数 cadence バケツへの同時刻印(1 issue が複数 rule に一致するケースの本対応 — `runs.cadence_label` の join table 化。v1 は fail-closed で明示的に締め出す)。
- 設定可能な TZ / UTC オフセット(#146 と同様に deferred)。
- `meguri ps` への待機表示(上記「主要な決定 1」。必要なら観測テーブル方式で follow-up)。
- Phase 4 remote TaskSource での消化カウント権威の再配置(ADR 0003 / 0011)。
