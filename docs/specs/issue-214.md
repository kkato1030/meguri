# spec: issue #214 — self-review slice 3: round 1 の並列 reviewer 化

> 使い捨ての足場。実装が landing したら削除する。恒久的な設計判断は ADR 0023
> (並列 reviewer の設計)/ ADR 0024(信頼境界)へ、計測境界は ADR 0020 へ既に振り分け済み。

## spec 深度と選定理由

**design spec(深い方)を選ぶ。** 未決定が多く(verdict 合成・スロット重み式・reviewer 命名)、
爆発半径が広い(完了コントラクト・スケジューラ並行性・config schema・checkpoint 永続・新設の
injection 面)。checkpoint 永続状態(台帳の reviewer 属性)と、per-turn 完了コントラクトという
コントラクトに触れるため、**veto ルールで migration & rollback は必須**。

## 何を作るか(概要)

self-review の **round 1 だけ**を、`[[review.reviewers]]` で定義した N 本の reviewer に
並列 fan-out する。merge は Rust 側の機械的 union。round 2+ は ADR 0022 のまま単独 anchor
reviewer。未指定なら現行の単一 reviewer で **byte-for-byte 挙動不変**。

設計の柱は ADR 0023(発散はヘテロ・収束はホモ / union / needs_human 非 OR / findings 上限)と
ADR 0024(外部 finding の injection 面)。本 spec は「どこを・どう触るか」に集中する。

## 受け入れ観点(acceptance criteria)

- [ ] `[[review.reviewers]]` 未指定で、round 1 を含む self-review 全体が現行と byte-for-byte 同一
      (checkpoint JSON・イベント列・生成ファイル名まで)。
- [ ] `[[review.reviewers]]` 指定時、round 1 が N 本並列で走り、各 reviewer が独立ファイルに書く。
- [ ] merge が全 reviewer の findings を決定的順序で union し、台帳に open として畳む。
- [ ] 並列 reviewer のいずれかが `needs_human` を返しても即 escalate せず、anchor 確認 turn を
      1つ挟む(anchor 肯定→escalate / 否定→続行)。
- [ ] round 2+ は `[[review.reviewers]]` の有無に関わらず単独 anchor reviewer。
- [ ] `[[review.reviewers]]` は **host-only**。repo `meguri.toml` からは書けない(`RepoConfig` に
      入れず、`deny_unknown_fields` で parse エラー→doctor が報告)。`profile` は外部モデルへの
      信頼の宣言なので、`[agents]`/`[routing]` と同じ host 専用境界に置く(§host-only)。
- [ ] 並列 turn どうしが `.meguri/result.json` で衝突しない(review turn は `result-<turn_id>.json`)。
- [ ] スケジューラが並列 reviewer 分のスロットを予約し、`max_concurrent` を越えない
      (重み = `max(advisor_weight, N_reviewers)`)。
- [ ] 各 round 1 reviewer の prompt に「1本あたり最大 5 件(K=5)」の findings 上限が入る。
- [ ] reviewer profile の detect 失敗はその1本を落として続行(全滅時は単独 anchor へフォールバック)、
      run を止めない。
- [ ] doctor が codex / grok 等の profile を誤って ❌(ModelInvalid)にしない。
- [ ] profile 別の unique 貢献率 / waive 率が #213 で観測できる(finding ごとに reviewer 属性)。
- [ ] テスト:merge・needs_human 確認 turn・result per-turn 化(FakeMux/FakeForge)+ 統合テストは
      `fake_agent.sh`。
- [ ] `cargo fmt --check` / `clippy -D warnings` / `nextest run` / `test --doc` が緑。

## 触るファイル(files to touch)

- `src/config.rs` — `ReviewConfig` に `reviewers: Vec<ReviewerConfig>`(`[[review.reviewers]]`)を追加。
  `ReviewerConfig { profile: Option<String>, lenses: Option<Vec<String>> }`。空で現行値。
  **`RepoConfig` には足さない**(`[review]` は今も RepoConfig 外＝host-only。`deny_unknown_fields` が
  repo `meguri.toml` の `[review]` を parse エラーにする既存挙動を維持)。
- `src/engine/self_review.rs` — round 1 の並列 fan-out / merge / needs_human anchor 確認を追加。
  `review_turn` の単独経路は温存し、非空 `reviewers` のときだけ新経路へ分岐。`Finding` /
  `LedgerEntry` に reviewer 属性(`reviewer_profile` 等、`#[serde(default, skip_serializing_if)]`)。
- `src/turn/prompts.rs` + `src/turn/mod.rs` — per-turn 完了コントラクト(`result-<turn_id>.json`)を
  review turn 限定で選べる形に。`prepare_turn` / `read_result` / `clear_result` / `completion_contract`
  に isolated 変種。既存の固定 `result.json` 経路は不変。
- `src/engine/flow.rs` — `self_review_lane` を「N 本の並列 reviewer lane」を作れる形に拡張
  (`run_review_turn` の per-reviewer 版 / lane キーの一意化)。
- `src/engine/scheduler.rs` + `src/collab.rs`(or 新 predicate) — `run_weight` に並列 reviewer 分を
  加算(`run_gets_advisor` の前例に倣う predicate + 重み一致)。
- `src/routing.rs` — `probe_profile` を command basename 別の probe 戦略に一般化
  (未知 CLI は `Unavailable` のまま、誤 `ModelInvalid` を出さない)。
- `src/store/panes.rs` — 並列 reviewer 用の lane キー(`self-review#<i>` 等)。
- `tests/` — 統合テスト(`fake_agent.sh` を複数 reviewer に対応)。
- ドキュメント — 設定例(`[[review.reviewers]]`)を該当の config リファレンスへ。

## 主要な決定(key decisions)

これらは review で収束させたい判断点。ADR 0023/0024 の原則を実装レベルに落とした結果。

1. **並列は round 1 のみ。** round 2+ と decision 裁定は anchor(self-reviewer profile)固定。
   `reviewers` 設定は round 1 の fan-out にのみ効く。
2. **byte-for-byte は「非空 `reviewers` でのみ新経路」で担保。** reviewer 属性は
   `skip_serializing_if = "Option::is_none"` で単独経路の checkpoint を汚さない。単独経路は
   既存 `review_turn` をそのまま呼ぶ(finding id 採番・イベント・ファイル名すべて不変)。
3. **reviewer 命名 = 決定的 index。** ファイルは `self-review-r<i>.json`(i は設定順)。
   単一モデル lens 分割では profile が同一なので profile を名前に使えない。属性(profile・lens)は
   イベント/台帳に別途持たせる。
4. **merge = 連結 union。** dedup しない(recall 優先・ADR 0020)。id は (reviewer index, finding order)
   の決定的順で採番。round 1 は台帳が空なので全 finding が新規 open。
5. **verdict 合成。** 全 clean→clean、いずれか fixable→fixable、いずれか needs_human→anchor 確認 turn
   を1つ挟む(§needs_human)。確認 turn は round を消費しない(round 1 内の解決)。
6. **needs_human anchor 確認。** flag した reviewer の `review` 散文を anchor に渡し、1 turn で判定。
   肯定→`EVENT_NEEDS_HUMAN` で escalate。否定→anchor の verdict/findings を union に畳んで続行。
   複数 flag でも確認 turn は1回。
7. **findings 上限 K = 5(定数)。** round 1 parallel prompt に「1本あたり最大 5 件」を入れる。
   初期は config 化しない(定数)。N 本 × 5 の union が fix prompt を薄めるほど膨らむと実測されたら
   後で config 化する。
8. **スロット重み = `max(advisor_weight, N_reviewers)`。** advisor は execute 中、並列 review は
   self-review 中に並行し**時間帯が重ならない**ので、和ではなく max を取る(ピーク同時 agent 数)。
   `run_weight`(`src/engine/scheduler.rs`)を、`run_gets_advisor` 由来の重みと round 1 reviewer 本数の
   大きい方にする。単独 reviewer(空 `reviewers`)は現行どおり advisor_weight のまま(=1、advisor 時 2)。
9. **reviewer profile 解決失敗 = その1本を落として続行 + イベント記録。** recall 目的なので detect 失敗の
   1本で run を止めない。全本が失敗したら単独 anchor(self-reviewer profile)へフォールバックし、
   `self_review.reviewer_dropped { profile, reason }` を emit する。run 失敗にはしない。
10. **profile と launch mode は別々に解決する。** reviewer が使う CLI・モデル(profile)は
    `[[review.reviewers]].profile` から選ぶ(省略時は self-reviewer role の routing profile へフォールバック)。
    一方 launch mode は現行 `self_review_lane` どおり **`self-reviewer` role** で解決する
    (`launch::resolve(&config, "self-reviewer")` — profile 名を launch role として使わない)。
    つまり各 reviewer は「設定された profile で起動しつつ、起動方式は self-reviewer role 共通」。
    self-review は one-shot なので `direct` 推奨(ADR 0012)。並列でも lane キーを一意化すれば
    pane モードでも衝突しない。

### host-only 境界

`[[review.reviewers]]` は host `config.toml` の `[review]` 配下にのみ書ける。`profile` は
`[agents.profiles]` の名を引き、どの(外部)モデルを reviewer として信頼するかの**信頼の宣言**で
あるため、ADR 0011(二層 config)の「信頼の宣言は host 専用」原則と ADR 0024 の「信頼できる profile を
設定者が選ぶ」前提に従う。実装上は現状追認で足りる:`RepoConfig`(`src/config.rs`、`deny_unknown_fields`)は
`[review]` を持たないので、repo `meguri.toml` に `[[review.reviewers]]` を書くと既に parse エラーになり
doctor が報告する。この境界を spec/ADR に明記し、`RepoConfig` へ `review` を**足さない**ことを不変条件とする。

## アーキテクチャ影響(architecture impact)

- self-review phase は「単独 review turn → fix turn の直列ループ」から、「round 1 = fan-out+merge、
  round 2+ = 単独」の二相へ。`self_review_inner` のループ先頭に round==1 かつ非空 `reviewers` の分岐を
  足す。台帳・cap・ping-pong・final-fix(ADR 0022)は round 2+ 側でそのまま生きる。
- 完了コントラクトが2形態になる:固定 `result.json`(既存全経路)と per-turn `result-<turn_id>.json`
  (並列 review turn のみ)。`read_result` は turn_id で中身を照合済みだが、**ファイル名の衝突**が
  並列で問題になるため、ファイル名側を per-turn にする。単独経路は固定名のまま。
- lane 抽象は「issue-scoped 1本」から「並列 N 本の review lane」へ。pane/session キーの一意化が要る。
- スケジューラの重み会計に self-review の並行本数が入る。これまで self-review の reviewer 1本は
  author pane idle 中に走るため重み未計上だったが、N 本同時はスロットを実際に食うので計上する。

## 代替案と決定(alternatives considered & the decision)

- **orchestrator agent に裁定させる** → 却下。裁定 agent 自身が較正点・幻覚点・帰属不能点になる。
  機械的 union(ADR 0023 §1)を採る。
- **needs_human を OR で即 escalate** → 却下。幻覚 escalate が N 倍(ADR 0023 §2)。anchor 確認 turn。
- **lens × model の行列で fan-out** → 却下。過剰・帰属不能(ADR 0023 §3)。単一モデルは lens 分割、
  複数モデルはモデル分割。
- **全 review turn を per-turn result 化** → 却下。単独経路の byte-for-byte が崩れる。並列 turn のみ。
- **外部 finding body を sanitize** → 今回は却下(ADR 0024)。waive 裁量 + 検証 + human merge を緩衝に。

## migration & rollback(veto により必須)

- **config**:`[[review.reviewers]]` は追加のみ。既存 config は空 `reviewers` として現行挙動。
  serde default で後方互換。
- **checkpoint(run step の永続 JSON)**:`Finding` / `LedgerEntry` の reviewer 属性は
  `#[serde(default, skip_serializing_if = "Option::is_none")]`。旧 checkpoint は属性なしで parse でき、
  単独経路は属性を書かないので JSON も不変。ADR 0022 の `mirror_open_to_pending`(rollback safety valve)は
  そのまま効く。
- **完了コントラクト**:per-turn `result-<turn_id>.json` は並列 review turn 限定。既存の全経路
  (author/fix/planner/worker/pr-reviewer/単独 review)は固定 `result.json` のまま — 外部から見える
  コントラクトは変わらない。
- **rollback**:`[[review.reviewers]]` を消す(or `review.enabled = false`)ことは、**新規に dispatch
  される run と、中断後に再 dispatch される run**にだけ効く。既に走っている run は dispatch 時の
  `Deps`(と、repo config を使う場合は claim 時に pin した設定)で最後まで進む(ADR 0011 の
  claim 時 pin と、スケジューラの「dispatch 済み run は spawn 時 Deps を保持」に従う)。
  設定変更を即座に全 run へ反映する経路ではない。DB schema 変更はなく、`[review]` は host-only なので
  rollback は host `config.toml` の編集で行う。

## observability

- **既存**:`self_review.reviewed`(round 集計)・`EVENT_CLEAN/UNCONVERGED/NEEDS_HUMAN/PINGPONG/FINAL_FIX`
  は不変。#213 の既存クエリはそのまま動く。
- **新規(段階導入・ADR 0020)**:finding ごとに reviewer 属性を台帳に持たせ、reviewer 単位のイベント
  (例 `self_review.reviewer_reported { profile, lenses, findings }`)を round 1 で emit。#213 が
  **profile 別 unique 貢献率**(その profile だけが出した finding の割合)と **waive 率**を出せる。
- needs_human anchor 確認 turn の結果(肯定/否定)も1イベント emit し、非 OR 経路が効いているかを測れる。

## test strategy

- **単体(FakeMux/FakeForge)**:
  - merge:複数 review ファイル → union の順序・id 採番・台帳への畳み込み。
  - verdict 合成:clean 揃い / fixable 混在 / needs_human 混在(→ anchor 確認 turn 差し込み)。
  - anchor 確認:肯定→escalate、否定→続行(union に畳む)。
  - per-turn result:2本の並列 review turn が別ファイルに書き衝突しない。turn_id 照合。
  - byte-for-byte:`reviewers` 空で checkpoint JSON・イベント列が現行と一致(スナップショット)。
  - scheduler:`reviewers` 有りの run が重み `max(advisor_weight, N_reviewers)` を予約し
    `max_concurrent` を越えない。空 `reviewers` は現行重みのまま。
  - reviewer drop:一部 profile が detect 失敗 → 落として続行、全滅 → 単独 anchor フォールバック、
    `self_review.reviewer_dropped` を emit(run は止まらない)。
  - findings 上限:round 1 parallel prompt に K=5 の上限文言が入る。
  - host-only:repo `meguri.toml` の `[[review.reviewers]]` が parse エラー(`RepoConfig` 拒否)。
  - routing:`probe_profile` が codex/grok basename を `Unavailable`(誤 ModelInvalid 無し)。
- **統合(`tests/*.rs` + `fake_agent.sh`)**:実 tmux・実 worktree・bare origin で、N 本の疑似 reviewer が
  それぞれ review ファイルを書き、merge→fix→(round 2 単独)→publish まで通す。needs_human 混在の
  anchor 確認経路も1本通す。

## スコープ外

- round 2+ の並列化(意図的に単独 anchor 固定・ADR 0023 §3)。
- 外部 finding body の能動 sanitize(ADR 0024)。
- 実行時の信頼度重み付け・finding 取捨(ADR 0020 = union 据え置き)。
- reviewer 編成の自動チューニング(採否は #213 の stats を見て人間がオフライン判断)。
