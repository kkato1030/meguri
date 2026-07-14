# issue-121 spec — collab 基盤の「測定」を第一級にする(`meguri stats collab`)

## この spec が閉じる範囲

issue #121 は #111 の**上位概念**として探索を置いた issue で、本文の後半「セルフレビューによる改訂」が最終形だ。そこに一行で書いてある:

> 協働面は速く弄れるが meguri には見えない。だから "テスト" を名乗る条件は、統率面の durable 信号で編成を比較する測定を同時に持つこと。step 1 は #111 の read-only advisor + 測定に絞り、編成 DSL は2例目まで封印する。

**step 1(read-only advisor)は #111 として既に landed している。** `src/collab.rs` と ADR 0006 が、advisor レイヤ本体・read-only 配線・スロット会計・run 終端 reap・DSL 封印まで固めた。残っているのは自己レビューが「真の欠落」と名指しした一点、**step 1.5(最重要):測定**だけである。

だからこの spec は編成 DSL もテンプレート externalize もローカルモードも作らない。それらは #111 で済み(advisor)、封印中(DSL は2例目まで §②)、別 issue(#54 ローカルは step 3)だ。**この spec の仕事は「協働の効き目を meguri の durable 信号で比較できるようにする」ことに絞る。**

一行の決定:**advisor が実行中の各 run に、その run を統べた collab の面(off / advisor)を durable に刻み、`meguri stats collab` でその面ごとに既存の統率面 durable 信号(成功率・turns・所要時間)を比較できるようにする。**

## spec 深度の理由(なぜ design spec か)

`runs` テーブルに列を1本足す = **永続状態のスキーマ変更**。採用ルールの veto 条項(永続状態・スキーマ・公開契約に触れるなら migration & rollback は必須)に該当するため、深い tier(design spec)を選び、下に migration / rollback / observability / test strategy を置く。不確実性そのものは低い(既存 `stats routing`(#65)の骨格をほぼ踏襲する)が、スキーマに触れる一点で floor を跨ぐ。

## 決定 1:何を測るか — 統率面の durable 信号のみ(協働面は測らない)

自己レビュー①の核心。協働面(agmsg)は「meguri が読まない=advisory」と設計した(ADR 0006)。その代償として、**協働の効果を協働面から測る手段はゼロ**だ(相談ログを読めば meguri の不変条件を破る)。よって効果は**統率面に既に durable に落ちている信号**でしか測れない。

v1 が測るのは `runs` テーブルに既にある run ローカルな信号だけにする:

- **成功率**(`succeeded` / scored)— advisor 有無で完了契約の通過率が変わるか。
- **平均 turns**(`turn_no`)— issue の «turns-to-green» の代理。ドリフトが早く摘まれれば turns は減るはず、という仮説の検証軸。
- **平均所要時間**(`finished_at - started_at`)。

issue が挙げた「fixer ping-pong 往復数 / impl-review findings 数 / CI 失敗回数」は run をまたいで PR 単位で集計する**別種の信号**で、`runs` 単独には無い。これらは後続の拡張とし、v1 では**作らない**(ADR 0001 最小主義・§② Rule of Three)。measurement の骨格を通し、信号は後から足せる形にしておく。

この「協働面は不可視ゆえ、効果は統率面 durable 信号でのみ測る」という判断は spec より長生きするので **ADR 0017**(本 PR 同梱)に置く。

## 決定 2:何を group key にするか — collab の面(ensemble ではない)

issue は `meguri stats ensemble` 相当と書いたが、§② で "ensemble" は封印した(具体パターンが advisor 1つだけの段階で framework を建てない)。だから group key に **"ensemble" という語を持ち込まない**。今日実在する軸だけで割る:

- key = `(project_id, loop_kind, collab_mode)`。`collab_mode` は `off` / `advisor`。
- コマンド名は `meguri stats collab`(`stats routing` の隣。`ensemble` を名乗らない)。

同じ役割(worker / spec-worker)の `off` 行と `advisor` 行を並べて比較する — これが「テスト」の実体だ。2例目の編成が出て初めて "ensemble" へ一般化する(その時に別 issue)。

## 決定 3〜:planning で切る4分岐への答え

1. **測定はこの issue に含める。** 自己レビューが「step 1 と同時(最重要)」と指定し、#111 が測定なしで landed した以上、#121 が測定を足す当の issue。#65 の `stats.rs` 基盤に相乗りする(束ねも独立もしない)。
2. **テンプレート提供の物理形は scope 外。** プロンプト本文 externalize は測定の core ではなく、`[prompts]` preamble(ADR 0012)で部分的に足りている。この issue では触らない。
3. **主戦場は実 GitHub issue。** #54 ローカルは step 3(別 issue)。測定は run が実在すれば source を問わず効く。
4. **編成は run 単位で固定。** `[collab]` は startup pin(hot-reload されない、`config.rs` の doc 参照)。daemon の生存中は面が固定で、run ごとに「その時どちらだったか」を刻む。途中乗り換えは無し(§⑥ と整合)。

## 変更箇所

### 1. スキーマ:`runs.collab_mode` を足す — `src/store/migrations/0015_collab_mode.sql`(新規)

```sql
-- 各 run を統べた collab の面(#121 測定)。advisor が付いた run にだけ
-- 'advisor' を書く。NULL = 書かれなかった(feature off / 非対象 / 旧 run)
-- = 集計側で 'off' と読む。routing_arm(0014)と同じ後付け nullable の流儀。
ALTER TABLE runs ADD COLUMN collab_mode TEXT;
```

### 2. 刻む:run launch 時に `collab_mode` を stamp — `src/store/runs.rs` + 呼び出し側

- `Store` に `update_run_collab_mode(&run_id, mode: &str)`(`update_run_routing_arm` と同型)。
- stamp するのは **`collab::run_gets_advisor(cfg, run)` が真のときだけ `"advisor"` を書く**。偽なら**何も書かない**(列は NULL のまま)。spawn の best-effort 失敗(ADR 0006)とは独立に、**意図した面**を刻む — 比較したいのは編成であって spawn の当たり外れではない。
- 「off を刻まず NULL のまま」にするのは inert 規律のため:`[collab]` 未指定なら `run_gets_advisor` は常に偽で、この列への書き込みは**一切発生しない**。よって feature off の DB は現状とバイト単位同一を保つ(集計は下記のとおり NULL を `off` と読む)。
- 呼び出し位置は routing_arm を stamp するのと同じ dispatch 経路(`src/engine/` の run 起動地点。既存の arm stamp を grep して隣に置く)。

### 3. 集計:`collab_stats` を stats.rs に足す — `src/store/stats.rs`

- `scored_outcomes` の projection に `COALESCE(collab_mode, 'off')` を1列足す(NULL = 書かれなかった run = 面が無かった=off として素直に読む)。
- `collab_stats(project, window) -> Vec<CollabStatRow>`:key を `(project_id, loop_kind, collab_mode)` にして `WindowAgg`(既存)で集計。ただし対象を**「advisor が付きうる run」だけに絞る** — これは `run_gets_advisor` の3条件のうち load-bearing な2つ:(a) advisor 対象ループ(`collab::supports_advisor_loop_kind` = worker / spec-worker)かつ (b) **GitHub issue backed**(`issue_number IS NOT NULL`)。local task の worker/spec-worker は advisor を受け取れない(team 名が issue scope、ADR 0006)ので、`off` 基準を汚さないよう集計から外す。この絞り込みは `scored_outcomes` に条件を足すのではなく `collab_stats` 側で行う(routing 集計は絞らないため)。
- `routing_stats` のバケット処理をほぼ流用。共通化しすぎず、`WindowAgg` / `scored_outcomes` の再利用に留める(routing の arm 分割ロジックは持ち込まない)。

### 4. CLI:`meguri stats collab` — `src/cli.rs`(`StatsCommand` に `Collab`)+ `src/app.rs`(`cmd_stats_collab`)

`cmd_stats_routing` と同じ表描画。列は `PROJECT / ROLE / COLLAB / RUNS / SUCCESS / AVGTURNS / AVGDUR`。行が無ければ `no collab stats yet`。

### 5. ドキュメント微修正

- `src/collab.rs` のモジュール doc か README に「測定は `meguri stats collab`」の一行(既存慣習に合わせる)。

## Architecture impact

新しい面・新しい制御経路は増えない。measurement は**読み取り専用の派生ビュー**で、`stats routing`(#65, ADR 0002 の「serve は sqlite 直読み」)と同じく watch 停止中でも動く。唯一の書き込みは advisor が付く run の1列 stamp(`"advisor"` のみ)で、これは routing_arm の stamp と同じ性質(意思決定に使わない観測用メタデータ)。feature off なら書き込みゼロ。完了契約・検証・scheduler には一切食い込まない(横断制約:協働面は完了契約を構造的に汚せない、を測定も守る)。

## Alternatives considered

- **`meguri stats ensemble` として編成軸で作る** → §② の封印違反。実在するのは advisor 1面のみ。`collab` 軸に留める。
- **列を足さず、`stats` 時に config から面を逆算** → config は startup pin で run より後に変わりうる。過去 run が「その時どちらだったか」は逆算できない。durable な事実は run 行に刻むしかない(routing_arm と同じ判断)。
- **fixer ping-pong / review findings まで v1 で測る** → run をまたぐ集計で `runs` 単独に無い。骨格を先に通し、信号は後付け(§②)。

## Migration & rollback

- **Migration:** `ALTER TABLE runs ADD COLUMN collab_mode TEXT`(0015)。既存 run は NULL、集計側で `off` に読む。追加のみで破壊なし・前方互換(routing_arm 0014 と同一手口)。列の追加自体は不可避のスキーマ変化だが、**行の中身**は feature off なら一切書き換わらない。
- **Rollback:** SQLite は `DROP COLUMN` を避ける流儀(既存 migration 参照)。ロールバックは「列を無視する」= 旧バイナリは `collab_mode` を SELECT しないので害なし。`[collab]` 未指定なら stamp は**発生しない**(`"advisor"` すら書かれない)ので、行データは現状とバイト単位同一を保つ。データ損失経路なし。

## Observability

`meguri stats collab` そのものが観測面。`stats routing` と同じ sqlite 直読みで、daemon 稼働と独立。新規の event 種別・notify・drift 判定は**足さない**(routing の drift は成績劣化アラートで別目的。collab は人間が編成を比較するための素の表に留める)。

## Test strategy

`src/store/stats.rs` の既存テストパターン(`seed_run` ヘルパ)を踏襲し、in-memory Store に対して:

1. 未 stamp(NULL)の issue worker run 群と `advisor` の issue worker run 群を seed → `collab_stats` が `off`/`advisor` の2行に割れ、それぞれ success_rate / avg_turns が正しい。
2. `collab_mode` NULL(未 stamp / 旧 run)が `off` 行に読まれる。
3. `planner` など非対象ループ、および **local task の worker/spec-worker(`issue_number` NULL)は `collab_stats` に**出ない**(loop-kind フィルタ + issue-backed フィルタの両方)。
4. window が新しい N 件だけを見る(routing と同じ)。
5. `run_gets_advisor` が真なら `"advisor"` が stamp され、偽なら列は NULL のまま(書き込みが起きない)ことを確認(stamp 地点の単体 or 結合テスト)。

`cargo fmt --check` / `clippy -D warnings` / `nextest` / `--doc` を通す(CI 同順)。

## 受け入れ基準

1. `runs.collab_mode` 列が migration 0015 で足され、`run_gets_advisor` が真の run にだけ `"advisor"` が stamp される(偽なら NULL のまま)。
2. `meguri stats collab` が `(project, role, collab_mode)` 別に成功率・平均 turns・平均所要時間を表示し、watch 停止中でも動く(sqlite 直読み)。
3. 集計は **issue-backed の worker / spec-worker に絞られ**(loop-kind + `issue_number IS NOT NULL`)、非対象ループも local task も出ない。未 stamp(NULL)は `off` として読む。
4. `[collab]` 未指定なら `collab_mode` への書き込みは一切発生せず、行データは現状とバイト単位同一(inert 規律)を保つ。
5. 決定 1(統率面 durable 信号でのみ測る)を ADR 0017 に記録。編成 DSL・ensemble 語・cross-run 信号は v1 に**入れない**(封印を明文化)。
6. CI 4点(fmt / clippy / nextest / doc)通過。作業ツリー clean。
