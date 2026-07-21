# spec: issue #246 — escalation の冪等化（read-after-write + comment dedup）

- Issue: #246
- 設計書: `docs/design/needs-human-friction-and-delivery-speed.md` §3-C / §P2
- 設計判断: ADR 0028（エスカレーションの冪等性）
- **spec 深度: normal**。理由: 設計は設計書 §P2 と ADR 0028 で確定しており、永続 state・
  schema・migration には触れない（veto の migration 節は不要）。ただし全 escalation 経路が
  通る横断的な contract なので blast radius は広い。undecided は「どこを単一ゲートにするか」
  「dedup キーの取り方」の 2 点で、いずれも下記で確定させた。

## 何を・なぜ

level-triggered なエスカレーションが、ラベル書き込み失敗 / 観測キャッシュの stale 読みで
重複発火し、同一 PR に同一文面の needs-human コメントが複数付く（実例: PR #231 に 3 件）。
ラベル書き込みの結果を捨てず、コメントを head×reason マーカーで dedup することで、
「PR head × reason ごとに escalation コメントは高々 1 件」を forge 状態だけで保証する。

## 確定した決定（open だった A-or-B を全て確定）

1. **単一ゲートの置き場所**: 修正は `src/engine/escalation.rs` に集約する。ガード済み
   プリミティブ（read-after-write のラベルゲート + head×reason dedup）を 1 箇所に置き、
   全 PR escalation 経路をそこに通す。各呼び出し側に散らさない。
2. **`escalate_pr` のシグネチャ**: `escalate_pr(deps, pr: i64, reason: &str, comment: &str)`
   に `reason` slug を追加する。head は **呼び出し側から渡さず `escalate_pr` 内で
   `get_pr` から取る**（観測キャッシュの stale head を dedup キーに使わないため）。
   呼び出し側の変更は reason slug を 1 つ渡すだけに留まる。
3. **dedup キー**: `head`（`get_pr` の現物）× `reason`（slug）。head 単独でも reason 単独でも
   ない（ADR 0028 の理由参照）。
4. **マーカー書式**: `<!-- meguri:needs-human reason=<slug> head=<sha> -->` をコメント先頭に
   埋める。arm / claim マーカーと衝突しない prefix。`escalation.rs` に
   `needs_human_marker(reason, head_sha)` として定数化。
5. **マーカーの権威**: dedup 判定は `pr_comments_meta` を読み、`viewer_did_author == true` の
   コメントだけを信頼する（第三者による偽造抑止を防ぐ、ADR 0027 と同じ threat model）。
   `pr_comments`（body だけ）ではなく `pr_comments_meta` を使う。
6. **順序と失敗時の扱い**（`escalate_pr` 内）:
   1. `get_pr` で現 head を取る → Err なら `escalation.deferred` を emit して return。
   2. `pr_comments_meta` を読む → Err なら defer して return。
   3. `add_pr_label(needs-human)` → **Err なら `escalation.deferred` を emit して return
      （コメントも通知も出さない）**。これが read-after-write ゲート。
   4. `remove_pr_label(working)` は best-effort（従来通り）。
   5. マーカーが自己投稿コメントに既にあれば `escalation.deduped` を emit して return
      （ラベルは 3 で担保済み、コメントと通知だけ抑止）。
   6. なければ `pr_comment(marker + "\n" + comment)`、`escalation.raised`（`reason` を
      payload に追加）、`notify(escalation_pr)`。
7. **reason slug 一覧**（安定・原因ごとに区別）:
   - reconciler 予算切れ conflict → `conflict_budget`
   - reconciler 予算切れ ci → `ci_budget`
   - reconciler pr-review 失敗 → `pr_review_failed`
   - reconciler stuck backstop → `stuck`
   - ci_fixer ターン escalate → `ci_fixer`
   - conflict_resolver ターン escalate → `conflict_resolver`
   - fixer ターン escalate → `fixer`
   - spec_worker ターン escalate → `spec_worker`
   - pr_reviewer escalate（review 未完了 / impl blocking）→ `pr_review`
   - spec_fixer 予算切れ → `spec_review_parked`
8. **spec_fixer の別経路**: `spec_fixer::escalate_budget_exhausted` は現状 `escalate_pr` を
   通さず自前で `add_pr_label` + `pr_comment` + `awaiting_human` 通知している。通知の型
   （`awaiting_human` + 合成 run キーの throttle）を保ちたいので、**ラベルゲート + dedup の
   コア部分だけを共有ヘルパに切り出して spec_fixer からも呼ぶ**。案: `escalation.rs` に
   `park_pr_needs_human(deps, pr, reason, comment) -> ParkOutcome`
   （`Posted` / `Deduped` / `Deferred`）を置く。`escalate_pr` はこれ + `escalation_pr` 通知、
   spec_fixer は `Posted` のときだけ既存の `awaiting_human` ページを送る。
9. **`escalate_issue` の扱い**: 本 issue の実例（PR #231）は PR 経路。issue 直接経路
   （`escalate_issue`）は同型の level-triggered 重複を持ちうるが、受け入れ基準の対象外・
   別 reason 空間なので**本 issue では変更しない**（必要なら別 issue）。スコープを PR 経路に
   限定する。

## 触るファイル

- `src/engine/escalation.rs` — `park_pr_needs_human` ヘルパ + `needs_human_marker` 追加、
  `escalate_pr` に `reason` を追加してヘルパ経由に。`escalation.deferred` /
  `escalation.deduped` イベント追加。単体テスト追加。
- `src/engine/issue_reconciler.rs` — `escalate_budget_exhausted` / `escalate_pr_review_failed`
  / `escalate_stuck` の `escalate_pr` 呼び出しに reason slug を渡す。
- `src/engine/ci_fixer.rs` / `conflict_resolver.rs` / `fixer.rs` / `spec_worker.rs` /
  `pr_reviewer.rs` — 各ターン escalate の `escalate_pr` 呼び出しに reason slug を渡す。
- `src/engine/spec_fixer.rs` — `escalate_budget_exhausted` を `park_pr_needs_human` 経由に。
- `src/forge/fake.rs` — テスト支援: `add_pr_label` を**1 回だけ**失敗させる one-shot fault
  hook（例: `fail_add_pr_label_once(pr)`、カウンタ backing）を追加。既存の fault セットは
  永続なので、受け入れ基準の「1 回失敗」には one-shot 版が要る。

## 受け入れ基準

- FakeForge で `add_pr_label` を 1 回失敗させても、対象 PR の escalation コメントが高々 1 件
  であること（sweep を複数回回す単体テスト）。
- ラベルが付いた状態で観測が stale（`human_stop = false`）でも、同一 head×reason では
  2 件目のコメントも 2 通目の通知も出ないこと。
- 第三者（`viewer_did_author == false`）が偽マーカーコメントを置いても escalation が
  抑止されないこと。
- 新しい head を push した後は、同一 reason でも再エスカレーション（コメント 1 件）できること。
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run` /
  `cargo test --doc` が通ること。
