# issue-213 spec — `meguri stats review`: cap 率・round 分布を profile 別に測る

## 一行

自己レビュー(`src/engine/self_review.rs`)が既に `events` に落としている
`self_review.*` を `runs` に join し、`meguri stats review` で **cap-escalation 率**と
**clean 到達 round の分布**を `(project, loop_kind, agent_profile)` 別に読み出す。
`meguri stats collab`(#121)と同じ sqlite 直読みの派生ビューで、watch を止めていても動く。

## spec 深度の理由(normal)

読み取り専用の派生ビュー1本 + CLI サブコマンド1つの追加。永続状態・スキーマ・公開契約
のいずれにも触れず(既存イベントを読むだけ、列は増やさない)、不可逆リスクも無い。よって
migration/rollback 節は不要(veto ルール非該当)。未決事項は「終端イベントの数え方」
「profile がどの run のものか」「台帳/並列指標の段階導入」に限局しており、それぞれ下の
「主要な決定」で畳めるので **normal** で足りる。

## 背景の事実(調査で確認)

- 自己レビューは worker / spec-worker / planner の3ループで走る(各 `Flavor::self_reviews()` = true。
  `worker.rs:80` / `spec_worker.rs:203` / `planner.rs:158`)。ベースラインの
  planner 64% / spec-worker 19% / worker 11% はこの `loop_kind` 軸に対応する。
- 1つの自己レビュー・フェーズは、必ず次の **終端イベントちょうど1つ**で終わる
  (`self_review.rs` の各 return を確認):
  - `self_review.clean` `{rounds}` — 収束して公開
  - `self_review.unconverged` `{rounds, pending}` — round cap に当たって escalate(= cap 落ち)
  - `self_review.needs_human` `{round}` — reviewer が人間判断を要求
  中断・pane 死は終端イベントを出さない(= フェーズ未完了。分母から自然に外れる)。
- `self_review.reviewed` `{round, verdict, findings}` は毎 round、
  `self_review.correction` `{problem}` は review turn の契約違反(tree を汚す/ファイル欠落)
  1回につき1つ出る。いずれも既存。
- events は `run_id` を持ち、`runs(id)` に `project_id` / `loop_kind` / `agent_profile` がある。
  集計はこの join でやる。自己レビューのイベントは **著者 run の `run_id`** で出る
  (review turn は `self-reviewer` profile の別レーンで走るが run は同じ)。したがって
  `runs.agent_profile` は **著者(worker 等)の profile** であって reviewer の profile ではない。

## 出すもの(このスライスの範囲 = ベースラインのみ)

`(project, loop_kind, agent_profile)` 群ごとに、既存イベントだけから:

- **phases**: 終端イベント総数(clean + unconverged + needs_human)
- **cap-escalation 率** = unconverged / phases
- **needs_human 率** = needs_human / phases(cap 落ちと人間要求を分けて見る)
- **correction 率** = `self_review.correction` 件数 / phases(reviewer の契約違反コスト。既存イベント)
- **round 分布**: clean フェーズの `rounds` 値のヒストグラム(1,2,3,…round で clean に到達した件数)

## 範囲外(該当イベントが無いので今は出さない)

台帳(#212)・並列(slice 3)が新イベントを入れてから、行/列/節の**追加**で足せる形にだけしておく
(今回クエリは書かない)。該当イベントが無ければ黙って出さない(受け入れ観点どおり):

- waive 率(reviewer false positive の代理)、ping-pong escalate 件数、decision finding の発生数と帰結
- reviewer profile 別の unique 貢献率 / waive 率 / 契約違反率 — これらは **reviewer の profile** が
  必要で、`runs.agent_profile`(著者 profile)からは出せない。台帳イベントが finding ごとに
  reviewer を持つようになって初めて出る。

## 主要な決定

1. **分母は終端イベントの三つ組。** フェーズは run につき高々1回、終端イベントちょうど1つで終わる
   ので、phases = count(clean)+count(unconverged)+count(needs_human)。distinct run_id を数える
   必要はない(resume は `self_review_converged` で短絡し二重に出さない)。
2. **profile 軸は著者 run の profile。** `runs.agent_profile` を使う(受け入れ観点どおり)が、その意味は
   「self-review を回した著者ループの profile」。reviewer profile 別の指標は台帳イベント待ち(範囲外)。
3. **窓は設けない(v1)。** routing/collab の「直近 N 件」窓と違い、これは編成判断のための累積スナップ
   ショットなので、記録済みの全フェーズを集計する(`--project` スコープのみ、他 stats と対称)。
   `--since`/`--window` は将来の別 issue。
4. **段階導入。** 台帳/並列指標はイベントが無い今は実装しない。集計構造(群 → 指標の map)を、
   後から event-gated な指標を足せる形に保つだけにする。
5. **join は INNER。** `run_id` を持たない/孤児の event は落とす(自己レビュー event は必ず run_id 付き)。

## 触るファイル

- `src/store/stats.rs` — `ReviewStatRow`(群キー + phases/cap率/needs_human率/correction率)と round
  ヒストグラム、`review_stats(project)` を追加。`self_review.*` を `runs` に join して集計。テストは
  同ファイルの `seed_run` 系ヘルパに event 発行を足して書く(FakeForge/mux 不要、Store だけで完結)。
- `src/cli.rs` — `StatsCommand::Review { project }` を追加(`Collab` の隣、同じ doc スタイル)。
- `src/main.rs` — `StatsCommand::Review { project } => app::cmd_stats_review(project.as_deref())`。
- `src/app.rs` — `cmd_stats_review`。`cmd_stats_collab` に倣った表 + round 分布の1行。空なら
  「no review stats yet」。
- `docs/adr/0020-self-review-measured-from-orchestration-events-union-merge.md` — 決定の記録(本 PR 同梱)。

README への追記は見送る(先行の `stats collab` #121 も README に節を足していない。対称を保つ)。

## 受け入れ基準

1. `meguri stats review` が `self_review.*` イベントだけから `(project, loop_kind, agent_profile)`
   別の phases / cap-escalation 率 / needs_human 率 / correction 率を出す(スキーマ追加ゼロ)。
2. clean フェーズの round 分布(何 round で clean に到達したか)が出る。
3. イベントが1件も無ければ「no review stats yet」で静かに終わる。
4. 台帳/並列由来のイベントが無い現状で、それら由来の指標は表示されない(存在時だけ出す設計)。
5. `--project` でスコープでき、無指定なら全 project を project 列付きで出す(`stats routing`/`collab` と対称)。
6. `cargo fmt --check` / `clippy -D warnings` / `nextest run` / `test --doc` が通る。

## テスト計画

`src/store/stats.rs` の `#[cfg(test)]` に、Store へ run を作り `self_review.*` を `emit` する
ヘルパを足して:

- clean/unconverged/needs_human を混ぜた群で phases と cap-escalation 率・needs_human 率が合う。
- `self_review.correction` を混ぜて correction 率が合う。
- 異なる `loop_kind`/`agent_profile` が別行に分かれ、混ざらない。
- `clean {rounds:1|2|3}` を複数入れて round 分布が数え上がる。
- 中断フェーズ(終端イベント無し)が分母に入らない。
- `--project` スコープが効く。
