# issue-132 spec — spec/impl ループの対称化(内部 self-review 必須 + guard 任意 + 検査履歴)

> 設計判断の恒久記録は **ADR 0008**(本 PR 同梱)。この spec は実装着地で削除される使い捨ての
> 足場で、収束用のチェックリスト(受け入れ基準・触るファイル・フェーズ)だけを持つ。

## 狙い(一行)

spec と impl を挙動レベルで対称化する: 両方が「**必須の内部 self-review(多角視点)** +
**任意の GitHub guard レビュー(commit status + PR 本文 `<details>`)**」を持つ、`kind = Plan|Impl`
の単一パラメタ化ループにし、ADR 0003(auto-merge)↔0006(内部レビュー)の隙間を塞ぐ。

目標フロー(ADR 0008 §Decision):

```
human: label plan/ready → ai: exec(kind) → ai: self-review×N(必須) → ai: PR
     → ai: guard review(任意, status+<details>) → merge: human=advisory / auto-merge=gate
```

## 決定事項(ADR 0008 の要約 + 本 spec で確定させた実装レベルの決定)

| 論点 | 決定 |
|---|---|
| ADR 番号 | **0008**(0007 は merge-watch が使用済み。次の空き番号) |
| self-review の必須化 | planner の `Flavor::self_reviews()` を **true** に。既存 `flow`/`impl_reviewer` の内部ループを多角視点へ拡張し、kind で prompt を出し分け |
| 多角視点レンズ | 既定 `correctness / tests / simplicity / security`。**1 review turn が全レンズを見る**(turn 数を増やさない最小形)。findings に任意の `lens` タグ。kind=Plan では文書観点に読み替え |
| guard | `spec_reviewer` を kind 付き `guard` に格下げ・一般化。guard(Plan)=現行 spec レビュー、guard(Impl)=新設。出力は **commit status + 本文 `<details>` のみ、inline スレッドは出さない**(fixer ping-pong 防止, ADR 0006) |
| guard 既定 | **guard(Plan)=ON**(現行挙動を保つ)/ **guard(Impl)=OFF**(opt-in・外部 bot 互換)。project × kind で上書き可 |
| commit status | 新 forge メソッド `set_commit_status(sha, context, state, desc)` = `gh api -X POST repos/{repo}/statuses/{sha}`(**greenfield**)。context = `meguri/self-review` / `meguri/guard-review`。粒度 = 最終 verdict 一行 |
| self-review status の貼付 | self-review は公開前(worktree)に走るので、**push 後(open-pr)**に確定 head sha へ貼る。PR 本文の `<details>` は `compose_pr_body` を拡張して埋める |
| auto-merge gate | auto-merger の arm 条件に「該当 kind の guard が有効なら `meguri/guard-review`=success」を追加。**failure→needs-human / 未到達→no-op リトライ / guard 無効→条件を課さない**(ADR 0007 のデッドロック罠回避) |
| ci-fixer 除外 | ci-fixer の fixable 判定から `meguri/` 接頭辞の status context を除外(advisory 赤 guard で空振り/誤昇格させない) |
| plan_delivery | project config `plan_delivery = separate | combined`(既定 **separate**)。`ProjectMode` に相乗りさせない独立キー |
| separate の受け渡し | spec/ADR PR は `Closes #N` を**使わず** `Refs #N`(マージで issue を閉じない)。マージ検出の掃引が `speccing → ready` を張替 → worker が拾う。`spec_worker` は combined のときだけ活きる |
| routing role | 内部 self-review = role `self-review`、外部 guard = role `guard`(spec/impl 同一モデルで管理, 要件 3)。旧 `impl-reviewer`→`self-review`、`spec-reviewer`→`guard` を deprecated alias で温存 |

## 触るファイル

**self-review(必須・多角視点)**
- `src/engine/impl_reviewer.rs` → self-review を kind 対応・N レンズ化(module 名は据え置き可、または `self_review.rs` に改称)。`review_prompt` にレンズ列挙と kind 分岐を追加。`Finding` に任意 `lens` を追加
- `src/engine/flow.rs` — `Flavor` に `kind()`(既定 Impl / planner override Plan)。self-review フェーズを planner にも通す。`compose_pr_body` に self-review `<details>` を追加。open-pr で `meguri/self-review` status を head へ貼付
- `src/engine/planner.rs` / `src/engine/worker.rs` — `self_reviews()` / `kind()` の設定

**guard(任意・kind 付き)**
- `src/engine/spec_reviewer.rs` → `src/engine/guard.rs` に一般化(kind パラメタ)。discover: Plan=`spec-reviewing` PR / Impl=`implementing` 相当の meguri PR(spec 系ラベルなし・CI green・head 未 guard・guard(Impl) 有効)。settle: `meguri/guard-review` status + 本文 `<details>` 追記(inline は出さない)。head マーカーで dedup(既存パターン)
- `src/engine/mod.rs` — `default_loops()` に guard(Plan)/guard(Impl) を登録(merge 近い順)。`role_for_loop` を guard 対応

**commit status(forge)**
- `src/forge/mod.rs` — `set_commit_status` を `Forge` trait に追加
- `src/forge/gh.rs` — `gh api -X POST repos/{repo}/statuses/{sha}` 実装。必要なら `meguri/guard-review` ラベル追加は不要(status は label ではない)
- `src/forge/fake.rs` — status ストア + guard/auto-merger/ci-fixer 連結テスト用の読み取り口

**auto-merge / ci-fixer**
- `src/engine/auto_merger.rs` — arm 条件に guard gate を追加(§決定事項どおり保守的分岐)。`opted_in` から kind(= 常に Impl)を解決し guard(Impl) 有効性を参照
- `src/engine/ci_fixer.rs` — rollup の fixable 判定から `meguri/` status context を除外

**plan_delivery / 受け渡し**
- `src/config.rs` — `PlanDelivery` enum + `plan_delivery` フィールド(既定 separate)。`ReviewConfig` に `lenses` と `guard {plan,impl}` を追加し per-project override(`Config::review_for` / `guard_enabled(project, kind)`)
- `src/engine/planner.rs` — separate では PR body を `Refs #N`(非クローズ)、combined では現行どおり
- 受け渡し掃引: `src/engine/reaper.rs`(または小さな新掃引)— マージ済み spec PR(head が issue を encode・issue が `speccing`)を検出し `speccing→ready` を張替、コメント

**設定/文書**
- `src/routing.rs` — `KNOWN_ROLES` に `self-review`/`guard`、`recommended_chain`、deprecated alias
- `README.md` / `README.ja.md` — 対称ループ・guard の任意化・検査履歴の置き場・plan_delivery を反映
- `docs/ops/github-settings.md` — `meguri/guard-review` を required check にすると human 側も厳密ゲートになる旨を追記(任意運用)

**テスト**
- 更新: `tests/spec_reviewer_test.rs`(→guard)、`tests/worker_test.rs` / `tests/planner_test.rs`(self-review 必須化)、`tests/auto_merge_test.rs`(guard gate)、`tests/ci_fixer_test.rs`(meguri status 除外)、`tests/spec_worker_test.rs`(combined 限定)
- 新規: `tests/guard_test.rs`(kind 別 discover/settle・status・inline を出さないこと)、plan_delivery separate の受け渡し

## 受け入れ基準

1. planner run が spec/ADR PR を開く**前**に、必須の内部 self-review(多角視点)を回す(`review.enabled=true` 時)。worker も同様に多角視点で回す。
2. self-review の verdict が push 後 head に `meguri/self-review` commit status として貼られ、ラウンド要約が PR 本文の折り畳み `<details>` に載る(生トランスクリプトは載せない)。
3. guard(Plan)=ON なら spec PR が現行相当にレビューされ、`meguri/guard-review` status + 本文 `<details>` が付く。guard(Impl)=ON なら実装 PR の head が独立レビューされる。**どちらも inline レビュースレッドを作らない**(同 FakeForge で fixer が反応しないことを確認)。
4. guard は project × kind で独立に ON/OFF できる。OFF の kind では guard status も `<details>` も付かない。
5. auto-merger は「guard(Impl) が有効かつ `meguri/guard-review`=failure」の PR を **arm せず `needs-human`** にする。guard 未到達は no-op でリトライ、guard 無効なら従来どおり arm(デッドロックしない)。
6. ci-fixer は `meguri/*` の status context を fixable として拾わない(advisory 赤 guard で空振り/誤昇格しない)。
7. `plan_delivery=separate`(既定): spec/ADR PR は `Closes #N` を含まず、マージしても issue が閉じない。マージ後、掃引が issue を `speccing→ready` に張替、worker が実装を別 PR で拾う。
8. `plan_delivery=combined`: 現行の spec-worker morph(同一ブランチ takeover, 1 PR)が従来どおり動く。
9. 既存テストが全て通る(特に fixer / conflict-resolver / merge-watch の非破壊、ADR 0006/0007 の不変条件)。
10. ADR 0008 に本設計が記録され、README(en/ja)が対称ループ・guard 任意化・検査履歴の置き場を説明する。

## フェーズ分割(同一ブランチで順に実装。必要なら PR を分けてもよい)

- **P1 — self-review の対称化・多角視点化**: `Flavor::kind()`、planner の self_reviews、N レンズ prompt、self-review status + 本文 `<details>`、routing role `self-review`。(基準 1,2)
- **P2 — guard(kind) の一般化**: `spec_reviewer`→`guard`、guard(Impl) discover/settle、`set_commit_status` forge メソッド、guard status + `<details>`、config `guard{plan,impl}`、routing role `guard`。(基準 3,4)
- **P3 — auto-merge gate + ci-fixer 除外**: arm 条件、needs-human 分岐、`meguri/*` 除外。(基準 5,6)
- **P4 — plan_delivery separate + 受け渡し**: config、非クローズ参照、マージ検出張替掃引、combined 温存。(基準 7,8)
- **P5 — 文書 + テスト仕上げ**: README、github-settings、ADR 微修正。(基準 9,10)

## 未解決の論点 / リスク

- **guard(Impl) と ADR 0006 の緊張**: 0006 は inline 実装レビューを内部化した。guard(Impl) は
  **サマリのみ・任意**で inline を出さないため 0006 を破らない(ADR 0008 §3 で明文化)。実装時、
  guard が誤って `create_pr_review` を呼ばないことをテストで固定する。
- **auto-merge + guard(Impl)=OFF**: この組み合わせでは穴 2 が残る(最終 head 未レビューで
  auto-merge)。ADR 0008 は「mechanism を用意し、閉じるかは運用者の選択」とした。`meguri doctor`
  で `auto_merge.enabled && !guard(impl)` を **warn**(fail ではない)する案は P3 の任意タスク。
- **separate の spec PR を誰がマージするか**: 既定では `spec-ready` は auto-merge の blocking
  ラベルのまま(= 人間が ADR をマージ)。完全自律にしたい要望が出たら別 issue。
- **多角視点を 1 turn/全レンズにするか N turn/レンズにするか**: 本 spec は turn 数を増やさない
  1 turn 案を既定にした。効果が薄ければ N turn 案へ切替可能(config 次第)。
