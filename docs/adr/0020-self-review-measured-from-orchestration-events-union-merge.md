# ADR 0020: 自己レビューの効き目は統率面イベントで測り、実行時 merge は無差別 union のまま

- Status: accepted
- Date: 2026-07-15
- Issue: #213(self-review 計測 slice 2)/ 親 #211
- 関連: ADR 0017(協働の効き目は統率面 durable 信号でのみ測る、#121)、ADR 0011(統合 diff の自己レビュー、#176)、
  ADR 0006(自己レビューは内部ループ・forge を触らない)

## 文脈

自己レビュー(`src/engine/self_review.rs`)は review→fix の ping-pong を worktree 内で回し、
forge を一切触らない(ADR 0006)。その効き目 — round cap で落ちる率、収束までの round 数、
将来の異種モデルアンサンブルで各 reviewer が実際に何を拾ったか — を測る手段が無ければ、
#212(self-review 再設計)の効果検証も、どのモデルを reviewer に採るかの判断もできない。
ADR 0017 が collab 面で確立した思想(不可視な面は統率面の durable 信号で測る)を、自己レビュー面へ
広げる必要がある。

## 決定

**自己レビューの効き目は、`self_review.*` イベント(統率面に既に落ちている durable 信号)だけで測る。**

- フェーズは run につき高々1回、**終端イベントちょうど1つ**で終わる:
  `self_review.clean` / `self_review.unconverged` / `self_review.needs_human`。よって
  「フェーズ総数」= この三つ組の件数、cap-escalation 率 = `unconverged` / フェーズ総数 と定義する。
  中断・pane 死は終端イベントを出さない(未完了フェーズは分母に入らない)。
- 集計は event を `runs` に join し、`(project, loop_kind, agent_profile)` 別に読む
  (`meguri stats review`、`stats routing`/`collab` と同じ sqlite 直読み)。ここでの
  `agent_profile` は **自己レビューを回した著者 run(worker/spec-worker/planner)の profile** で
  あって reviewer の profile ではない — イベントは著者 run の `run_id` で出るからだ。
- **ベースラインはスキーマ追加なしで既存イベントから出す。** 台帳(#212)・並列(slice 3)が
  新イベントを入れて初めて出せる指標(waive 率・ping-pong escalate 数・decision finding・
  reviewer profile 別の unique 貢献率など)は、該当イベントがある時だけ表示する段階導入とする。
  骨格を先に通し、信号は後から足せる形にする(ADR 0017 の帰結と同型)。

**そして stats は人間のオフライン判断にだけ使い、実行時の finding merge は無差別 union のまま据え置く。**
どの reviewer を採る/外すかは、蓄積した stats を見て人間がオフラインで決める。実行時に信頼度で
重み付けして finding を取捨することはしない — union のまま流し、recall を落とさない。

## 帰結

- 「テスト」を名乗る条件が揃う:cap 落ち率・round 分布・(将来)reviewer 別 unique 貢献率を並べ、
  編成の入れ替え(モデルの採否)を durable 信号の分布で比較できる。回すだけでなく評価できる。
- 自己レビューの不変条件は無傷。measurement は run 行と event の観測用メタデータを読む派生ビューで、
  完了契約・検証・scheduler には食い込まない。実行時の挙動(union merge)も変えない。
- reviewer profile 別の指標は `runs.agent_profile` からは出せない。それは著者 profile だからだ。
  slice 3 で台帳イベントが finding ごとに reviewer を持って初めて出せる — この境界を明示しておく。
- 観察データであって無作為化実験ではない(issue 難易度などの交絡は残る)。これは「群を固定した
  durable 信号の比較」であり因果の証明ではない、と ADR 0017 と同じく正直に位置づける。
