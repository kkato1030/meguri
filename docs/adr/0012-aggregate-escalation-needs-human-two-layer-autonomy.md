# ADR 0012: escalation を1箇所に集約し、人間対応が要る全経路に `needs-human` を貼る — self-review(自動修復層)と guard(人間ゲート層)の2層モデル + autonomy モード

- Status: proposed
- Date: 2026-07-14
- Issue: #176
- 関連: ADR 0003(auto-merge / arm-only)・ADR 0006(AI 実装レビューの内部ループ化)・ADR 0008(spec/impl 対称化・必須 self-review + 任意 guard)・ADR 0011(combined も self-review を通す)

## Context

meguri には「人間の対応が必要になったら **必ず** `meguri:needs-human` を貼る」という
不変条件がある。これがラベルの意味(ボールが人間に渡った)を支え、放置された run が
再発見でループしない歯止めにもなる。ところが実装がループごとに散らばっていて、
経路によって貼られたり貼られなかったりする。調査すると4つの穴が開いていた:

| トリガ | 現状 | 問題 |
|---|---|---|
| conflict_resolver が `MAX_RESOLVE_RUNS` 枯渇 | discover が静かに止まるだけ | 誰も人間を呼ばない。PR は CONFLICTING のまま放置 |
| guard(plan) findings | `spec-reviewing` を維持するだけ | 誰も spec を直さない。次の push を待つが push は来ない |
| guard(impl) findings | commit status = Failure のみ | auto-merge opt-in 時に auto_merger が拾う経路以外は無防備 |
| self-review が `max_rounds` 到達で未収束 | footer を付けて PR を開くだけ | 未収束の diff が人間を呼ばずに公開される |

さらに ADR 0006/0008 が整理した「自動修復」と「人間ゲート」の役割分担が、
escalation の観点では明文化されていなかった。full-auto を導入するなら
「どこまで無人で進め、どこで必ず人を呼ぶか」を1本の軸で決める必要がある。

## Decision

### 1. 「自動修復」と「人間ゲート」を別の層に置く(2層モデル)

escalation を、責務の違う2つの層に分けて考える。

**層1: self-review(内部・PR 前・必須) = 自動修復層**(ADR 0006/0008/0011)

- レビュー LLM の verdict を **3値化**する: `clean` / `fixable` / `needs_human`。
  レビュー LLM 自身が「機械的に直せるか、人間判断が要るか」を分類する。
- `clean` → 収束。公開へ進む。
- `fixable` → 修復ループを継続(自動)。round を1つ使う。
- `needs_human` → **即 escalate**。round を浪費しない。
- `max_rounds` 到達で未収束 → **escalate**(従来は footer を付けて公開するだけだった。
  これが穴の4つ目)。
- ここが「自動可能なら自動修復 / 判断必須ならラベル」を担う。**モード非依存** — 
  Attended でも Full でも同じく self-review が常時回る。

**層2: guard(外部 GitHub review) = 人間ゲート層**(ADR 0008)

- guard の findings は plan/impl とも **一律 `needs-human`**。full-auto でも同じ。
- **guard-fixer は作らない。** self-review を通り抜けて guard が拾う指摘は、
  自動修復で潰しきれなかった=人間判断が要る確度が高いものだと見なす。
- guard discover は `needs-human` の付いた PR を skip する。現状 impl 側だけが
  この skip を持つので、**plan 側にも足して対称にする**。

### 2. なぜ guard-fixer を作らないか(却下した代替案)

代替は「full-auto では guard findings を disposition 分類し、`fixable` は専用
guard-fixer で自動修復する」。これは採らない。

- **(a)** ADR 0006 は guard を summary-only(commit status + PR 本文 `<details>`、
  inline thread 無し)にして AI↔AI のレビューピンポンを意図的に消した。guard-fixer は
  そのピンポンを別の形で復活させる。
- **(b)** 自動修復は既に self-review が担っている。同じ diff を2度自動修復させる二重化で、
  guard が拾うのは「self-review をすり抜けた分」= 人間の目が要る確度が高い残差。
- **(c)** full-auto は「安全な段階を自動で進め、green なら auto-merge する」であって
  「人間ゼロ」ではない。guard は難しいケースで **意図的に人を呼ぶチェックポイント**。

### 3. escalation を中央ヘルパに集約する(P1 を1箇所で保証)

人間が要る全分岐を `src/engine/escalation.rs` の中央ヘルパに寄せる。ここが
`needs-human` を貼る唯一の経路になり、不変条件を1箇所で保証する。

- issue/local タスク向け: `task_source.escalate`(github = `needs-human` ラベル + コメント /
  local = `status=needs_human` + reason)を必ず通す。
- PR 向け: `needs-human` ラベル付与 + `working` 除去 + コメントを1関数に統一する
  (現状 guard.rs / conflict_resolver.rs / ci_fixer.rs / auto_merger.rs に散らばる同型コード)。
- conflict_resolver の枯渇と self-review の未収束も、この経路に寄せる。

### 4. autonomy モード(config)

`Autonomy { Attended, Full }` を追加する。既定は **Attended**。global 既定 + project
override の wholesale パターン(`review` / `pr` と同じ)で、`Config::autonomy_for(project)`。
hot-reload 対象。

- **実役割は1つに絞る**: 「green になった PR に auto-merge を arm するか」。
  Full = arm する(無人でマージまで到達しうる)。Attended = arm しない
  (green で止め、人間が最終マージする)。auto-merger の arm 条件に
  「`autonomy == Full`」を1つ足すだけ。
- **分類・自動修復・escalation はモード非依存。** self-review の3値化も、guard findings の
  `needs-human` も、枯渇/未収束の escalate も、両モードで同じに走る(P1/P4 は環境に
  よらず常に成り立つ)。autonomy が変えるのは「最後のマージを無人でやるか」だけ。
- `auto_merge.opt_in`(どの PR を対象にするか)とは **直交**する軸。autonomy は環境全体の
  「無人マージを許すか」ゲート、opt_in は PR ごとの適格性。doctor で不整合
  (例: `auto_merge.enabled=true` かつ `autonomy=Attended` = arm 先が無い)を warn するのは任意。

### 5. autonomy × disposition × budget → action の一覧

| サイト | 条件(disposition / budget) | Attended | Full |
|---|---|---|---|
| self-review | `clean` | 収束・公開へ | 同左 |
| self-review | `fixable`(round 残あり) | 自動修復を継続 | 同左 |
| self-review | `needs_human` | 即 escalate | 即 escalate |
| self-review | `max_rounds` 到達・未収束 | escalate | escalate |
| guard(plan) | findings | escalate | escalate |
| guard(impl) | findings | escalate | escalate |
| conflict_resolver | `MAX_RESOLVE_RUNS` 枯渇 | escalate | escalate |
| ci_fixer | `MAX_CI_FIX_RUNS` 枯渇 | escalate(現状維持) | 同左 |
| validate | `limits.validate_turns` 枯渇 | escalate(現状維持) | 同左 |
| turn watchdog | idle/runtime 超過 | escalate(現状維持) | 同左 |
| auto-merge | green + guard clean + threads 0 | **arm しない**(人間が merge) | **arm** |

要点: escalate は全行でモード非依存。モードで唯一分岐するのは最終行(arm するか)だけ。

## Consequences

- **不変条件が構造で守られる。** `needs-human` を貼れるのは `escalation.rs` だけになり、
  「経路によって貼り忘れる」余地が消える。新しい人間ゲートを足すときも、この1関数を
  通すことがレビューの合言葉になる。
- **guard(impl) の escalate が settle で起きる。** これにより auto_merger 側の
  guard-failed escalate はほぼ発火しなくなる(guard が既に `needs-human` を貼るため
  auto_merger は blocking ラベルで skip する)。auto_merger 側は外部 bot 由来の
  失敗 status に対する backstop として残す。
- **guard(plan) findings が spec を止める。** 従来「誰も直さない」まま放置されていた
  spec findings が、明示的に人間へ渡る。spec の自動 fix ループは持たない(ADR 0006 の
  ピンポン回避と同じ判断)。
- **auto-merge の既定挙動が変わる(移行注意)。** `autonomy` 既定が Attended のため、
  今 `auto_merge.enabled=true` で運用中の環境は、そのままでは arm されなくなる。
  無人マージを続けたい環境は `autonomy = "full"`(global か project)を明示する必要がある。
  auto_merge の既定は元々 `enabled=false`(ADR 0003)なので影響を受けるのは
  「明示的に auto-merge を有効化済み」の環境に限られる。doctor の warn で気づけるようにする。
- **self-review の verdict コントラクトが2値→3値になる。** レビュー LLM が書く
  `.meguri/self-review.json` の `verdict` に `needs_human` が増える。DB スキーマや
  永続状態の変更ではなく、毎ターン新しく生成されるプロンプト/成果物コントラクトの拡張。
