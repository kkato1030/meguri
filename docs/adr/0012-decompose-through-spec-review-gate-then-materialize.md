# ADR 0012: 分解は spec-review ゲートを通す — planner が「分解提案 spec」を出し、承認後に専用の materialization ステップが子 issue + GitHub dependencies を起こす

- Status: proposed
- Date: 2026-07-14
- Issue: #134
- 関連: ADR 0001(spec は使い捨て)・ADR 0005(2軸ラベル)・ADR 0008(spec/impl 対称ループ・plan_delivery)・ADR 0009(分解の起票スコープは config)・ADR 0010(適応的 spec 深度)

## Context

meguri には既に分解の機構がある(issue #24)。planner の execute ターンが `status: decompose` +
`children` で終わると、`on_decompose`(`src/engine/planner.rs`)が**その場で**子 issue を起こし、
`blocked_by` を張り、親のラベルを剥がす。だが**この経路には人間の承認ゲートが無い**。planner が
「分解が要る」と判断した瞬間、子 issue 群が確定して並ぶ。

一方で meguri は spec-first フロー(`meguri:plan` → 調査 → spec → spec PR → review → 承認)という
**承認ゲート付きの機構**を既に持っている(ADR 0008)。大きなオーダー(新 subsystem、複数
component 横断、段階 rollout)の分解は、実装 spec と同じかそれ以上に**分解の切り方そのものを人間が
レビューしたい**: どの子がどの親要求をカバーするか、依存 graph は正しいか、rollout 順は妥当か。
これらは「その場で確定」ではなく「提案 → 承認 → 実体化」であるべきだ。

必要な部品はほぼ揃っている: planner(調査 + 提案文書)、spec review(承認ゲート)、そして discovery が
GitHub ネイティブの `blocked_by` を既に尊重してスキップする(README / looper ADR-0004)。分解後の
rollout 順序制御は追加実装ゼロで既存ループに効く。**唯一足りないのは「分解提案を review ゲートに
通し、承認後に実体化する」繋ぎ込みだけ**である。

## Decision

**分解を spec-review ゲートを通す経路に一本化する。** planner は分解が要ると判断したら、その場で
子を起こすのではなく、**分解提案 spec**(reviewable な文書)を書く。spec PR は実装 spec と同じ
review ゲート(`spec-reviewing → spec-ready`、ADR 0008)を通る。承認後、**専用の軽量な
materialization ステップ**が子 issue を起こし、`blocked_by` を張り、各子に指定の phase ラベルを付け、
親を tracking issue 化する。

### 1. 分解提案 spec は「レビュー対象 = 実体化対象」を一致させる

分解提案 spec は人間向けの散文(親のゴール / 要求カバレッジ / 依存 graph / rollout 順 / 各子の完了
条件)に加えて、**機械可読な `children` ブロック**(title / body / kind / blocked_by)を持つ。この
ブロックが materialization の唯一の真実で、かつレビュー対象そのものである。「レビューした切り方」と
「起こされた子」が別表現に分裂しない — カバレッジのレビュー(親の要求がどの子で満たされるか)が
実効的な保証になる。子ごとの `kind`(`ready` / `plan` / `human`)は既存 `ChildIssue` をそのまま流用
する。深い spec が要る子は `plan` にできる。

### 2. materialization は「専用の軽量ステップ」— spec-worker の終端動作にはしない

materialization は純粋な forge 操作(issue 作成・`blocked_by` 付与・ラベル付け・親への
tracking 化)であり、コードも commit も worktree も生まない。spec-worker のモデル(ブランチを
takeover して実装 commit を積み、diff を self-review し、PR を morph する)とは**何も重ならない**。
よって materialization は handoff / reaper / auto-merger と同じ「watch poll に相乗りする軽量掃引」
として置く。combined / separate のどちらの `plan_delivery` でも一様に効く(自分のマーカーで
対象を選ぶため、delivery mode に依存しない)。

分解提案 spec PR は実装が無いので**マージされない**。materialization 完了後に PR を**未マージで
close** する(spec は使い捨て、ADR 0001 — default branch には残さない)。永続状態は起こされた
子 issue 群 + dependencies だけである。

### 3. 冪等性は「子 body の安定 key + 単一 commit point」で担保する(途中失敗の再開)

materialization が子を N 個作った後で落ちても、再実行が重複 issue を作ってはならない(**取り返しの
つかない操作** — 起こした issue は自動では消せない)。素朴に「子を作ってから親へ進捗マーカーを書く」
設計は、作成と記録の間にクラッシュ窓があり `create_issue` が冪等でないため二重作成を許す。よって:

- **「作成済みか」の正典は親の依存 graph にする**。子1件を安定 key(`提案の親 + index`)を body に
  入れて作成し、直後に親→子の `blocked_by` を張る。「idx i は作成済みか」は `blocked_by(parent)` を
  引いて各ブロッカーの key を照合して判定する — これは GraphQL 直リレーションで**強整合・作成直後でも
  読め・closed 子も含む**(子が human/worker に閉じられても関係は残る)。full-text search は
  インデックス遅延と open 限定の弱さがあるので正典にせず、作成⇄リンクの窓を埋める all-state の
  backstop に限定する。その窓は reserve-first(作成前に親へ予約印)で更に縮め、疑わしい時は
  **再作成せず次掃引へ defer**(二重作成 < bounded 遅延、取り返しがつかないので安全側)。親 body の
  台帳は人間可読な高速パスにすぎない。
- **commit point は1つ = 提案 PR を close すること**。「全子マーカーが揃った」は「子作成済み」で
  あって「materialization 完了」ではない(親 tracking 化や PR close の前に落ちうる)。materialization
  ステップは提案 PR が open の限り全手順(子起こし・依存・親 tracking 化・PR close)を毎回冪等に
  やり直し、PR を close して初めて対象から外れる。前進のみで、どの中断点からでも安全に再開する。

Authority 原則(forge が真実)どおり、進捗も完了判定も forge 側の状態(子の存在・PR の open/closed)に
置く。

### 4. 親は無ラベルの tracking issue に戻す(2軸モデル)

materialization は親の phase ラベルを剥がす(`meguri:plan` 等)。2軸モデル(ADR 0005)では
「無ラベル = 未 triage / tracking」であり、親は子を待つ tracking issue になる。親は全ての子に
対して `blocked_by` を張られ、forge の graph 上で可視に子を待つ。子が全部 close したら親を閉じる
のは**当面は人間**(既存の #24 decompose と同じ)。自動 close は将来枠。

### 5. planner の判断は in-context のまま(専用の判定ループを作らない)

実装 spec を書くか分解提案 spec を書くかは planner の in-context 判断。基準は「複数の独立 PR として
レビュー・ロールバックしたい変更か」。ADR 0010 の適応的 spec 深度(normal / design)と同じく、
prompt に出力型の選択肢を並べるだけで、コード側で分解要否を計算しない。

## Consequences

- 分解が承認ゲートを得る。切り方・カバレッジ・依存 graph・rollout 順を人間(または guard(Plan))が
  spec PR 上でレビューしてから子が起きる。
- 既存の即時 `status: decompose`(#24)は planner の分解経路としては**引退**する。子起こしの
  filing ロジック(`on_decompose` の中核)は materialization ステップが共有する形に切り出す。
  `TurnStatus::Decompose` の即時経路は planner prompt から外れ、materialization が同じ filing を
  ゲート後に呼ぶ。
- 分解提案 spec PR は spec-worker / handoff sweep から**除外**が要る(マーカー判定)。両者は
  実装 takeover / `speccing → ready` 張替を分解提案には適用してはならない。
- 材料化は取り返しのつかない forge 書き込みを含むため、kill-switch(config)・子 body の安定 key・
  単一 commit point(PR close)で運用リスクを抑える。分解は1レベルのみ(既存不変条件):起こした子は
  さらに分解できない。
- `Forge` トレイトに 2 メソッド追加 + 1 契約強化:PR を未マージで畳む `close_pr`(commit point、
  現トレイトに close 手段が無い)、key で既存子を照合する `find_issue_by_marker`(**all-state** — 
  closed 子も拾う backstop)、そして `add_blocked_by(_in)` を**冪等契約**にする(既存 edge の再追加は
  no-op 成功。毎掃引の wire 張り直しが失敗しないため)。
- delivery mode に依存しない一様な掃引にすることで、combined / separate 両方で同じ materialization が
  効く。

## Out of scope(将来枠)

- tracking 親の自動 close(子が全部 close したら親を閉じる)。当面は人間が閉じる。
- GitHub sub-issues 機能の利用(親表現は body チェックリスト + `blocked_by` に留める)。
- 複数 repository にまたがる分解の新スキーマ(起票スコープは ADR 0009 のまま、GitHub の
  issue + dependencies が唯一の永続表現)。
- 実行中 PR の自動分割。
