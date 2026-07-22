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
5. **マーカーの権威と担体**: dedup 判定は `pr_comments_meta` を読み、`viewer_did_author == true`
   のコメントだけを信頼する（第三者による偽造抑止、ADR 0027 と同じ threat model）。
   ここに現行実装の 2 つの穴があり、両方を本 issue で塞ぐ（finding f2 / f3）:
   - **f2（実 Forge）**: `GhForge::pr_comments_meta` は `gh pr view --json comments` を使い
     `viewerDidAuthor` を読まない（既定 false のまま）。このままだと meguri 自身の marker も
     第三者扱いになり本番で dedup が効かない。`pr_comments_meta` を **GraphQL 経路**
     （`observe_open_prs` / overflow 用の comment node parse が既に `viewerDidAuthor` を
     埋めている、gh.rs 55/70/188-211）へ切り替え、`viewer_did_author` を埋める。
     GraphQL レスポンス（`viewerDidAuthor` 真/偽の両方）を parse する単体テストを足す。
   - **f3（Fake Forge / 投稿担体）**: escalation の投稿は `pr_comment` ではなく
     **`comment_pr`** を使う（実 Forge ではどちらも `gh pr comment` で等価。FakeForge では
     `comment_pr` が meta view=`pr_comments` に `viewer_did_author=true` で記録し、
     `pr_comment` は legacy `comments` にしか積まないため、投稿と read view がズレて
     dedup テストが常に 2 件目を出す）。あわせて footgun を消すため
     `FakeForge::pr_comment` も meta view へ記録するよう寄せる。
6. **順序・失敗時の扱い・戻り値**（`park_pr_needs_human` 内。`escalate_pr` はこれを呼ぶ薄い層）:
   戻り値は `ParkOutcome`（`Posted` / `Deduped` / `Deferred`）。
   1. `get_pr` で現 head を取る → Err なら `escalation.deferred` を emit し `Deferred` で return。
   2. `pr_comments_meta` を読む → Err なら `escalation.deferred` を emit し `Deferred` で return。
   3. **`remove_pr_label(working)` を先に best-effort で外す**（finding f1）。`working` が残ると
      `pr_is_touchable`（`src/engine/mod.rs:314`）が `needs-human` より先に claim を見て次 sweep を
      skip するため、ラベル追加失敗時の再試行が成立しない。**claim を先に解放してから**
      needs-human ゲートへ進めば、追加が失敗しても次 sweep は working なし・needs-human なしを
      見てクリーンに再エスカレーションできる。
   4. `add_pr_label(needs-human)` → **Err なら `escalation.deferred` を emit し `Deferred` で
      return（コメントも通知も出さない）**。これが read-after-write ゲート。
   5. マーカーが自己投稿コメントに既にあれば `escalation.deduped` を emit し `Deduped` で return
      （ラベルは 3-4 で担保済み、コメントと通知だけ抑止）。
   6. なければ `comment_pr(marker + "\n" + comment)` を投稿し `Posted` を返す。`escalate_pr` は
      `Posted` のときだけ `escalation.raised`（`reason` を payload に追加）+ `notify(escalation_pr)`。
   - **f1 の残余**: `remove_pr_label(working)` 自身が失敗し `working` が残るケースは、
     reconciler の run-liveness ベースの stale-claim 回収（`live_claim` / `reclaim_stale_claims`、
     ADR 0027）が担う（escalate する run は terminal へ向かうので claim は stale）。discovery
     ループが `working` **ラベル**を信頼して skip する残課題は設計書 §3-F の別スコープとし、
     本 issue では「失敗経路でも claim を先に外す」ことで再試行を確保する所までとする。
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
10. **caller 固有イベントを `Posted` で gate する**（finding f4）: 現在 caller は `escalate_pr` の
    直後に固有イベントを**無条件**で emit している（`reconciler.budget_exhausted`
    /`issue_reconciler.rs:1005`、`pr.merge_watch_stuck`/`:1145`、`automerge.pr_review_failed`
    /`:1128`、`pr_review.escalated`/`pr_reviewer.rs:929`）。`Deferred`/`Deduped` でこれらが
    出ると「コメントも通知も無い発火」が成功として記録される。`escalate_pr` が `ParkOutcome` を
    返すようにし、**各 caller は自分の固有イベントを `Posted` のときだけ emit** する。
    spec_fixer の `spec_fixer.budget_exhausted` イベント・`awaiting_human` ページも同様に
    `Posted` gate。
11. **同時実行時の原子性**（finding f5, decision）: live read → comment は原子的でなく、2 つの
    sweep が同時に「marker 無し」を読めば両方投稿しうる。**採用する立場**: 「head×reason ごとに
    高々 1 件」の保証は **単一 instance の直列化されたエスカレーション前提**で成立する、と明記する。
    根拠 —（a）reconciler sweep は単一ループで PR を順に処理する、（b）fixer 家族のターン
    escalation は DB の家族横断 active-run インデックス（ADR 0027）で PR 単位に排他される、
    （c）これは既存の arm マーカー dedup（`head_already_armed` → `arm()` comment、同じく
    read-then-write 非原子）と同じ前提であり、新しい弱点を持ち込まない。クロス instance
    （Phase-4 マルチホスト）の同時 escalation は二重投稿しうるが、冪等な `add_pr_label` により
    両者とも needs-human へ収束するので害は bounded。この限界を ADR 0028 と受け入れ基準に記録する。

## 触るファイル

- `src/engine/escalation.rs` — `park_pr_needs_human`（`ParkOutcome` を返す）ヘルパ +
  `needs_human_marker` 追加、`escalate_pr` に `reason` を追加してヘルパ経由に・戻り値を
  `ParkOutcome` に。`escalation.deferred` / `escalation.deduped` イベント追加、
  `escalation.raised` に `reason`。単体テスト追加。
- `src/engine/issue_reconciler.rs` — `escalate_budget_exhausted` / `escalate_pr_review_failed`
  / `escalate_stuck` の `escalate_pr` 呼び出しに reason slug を渡し、**固有イベント
  （`reconciler.budget_exhausted` / `pr.merge_watch_stuck` / `automerge.pr_review_failed`）を
  `Posted` で gate**（f4）。
- `src/engine/ci_fixer.rs` / `conflict_resolver.rs` / `fixer.rs` / `spec_worker.rs` /
  `pr_reviewer.rs` — 各ターン escalate の `escalate_pr` 呼び出しに reason slug を渡す。
  pr_reviewer は `pr_review.escalated` を `Posted` で gate（f4）。
- `src/engine/spec_fixer.rs` — `escalate_budget_exhausted` を `park_pr_needs_human` 経由に。
  `spec_fixer.budget_exhausted` イベントと `awaiting_human` ページを `Posted` で gate（f4）。
- `src/forge/gh.rs` — `pr_comments_meta` を GraphQL 経路へ切り替えて `viewer_did_author` を
  埋める（f2）。GraphQL parse の単体テスト追加。
- `src/forge/fake.rs` — テスト支援:（a）`add_pr_label` を**1 回だけ**失敗させる one-shot fault
  hook（例: `fail_add_pr_label_once(pr)`、カウンタ backing。既存 fault セットは永続なので
  「1 回失敗」には one-shot 版が要る）。（b）`pr_comment` を meta view=`pr_comments` にも
  記録するよう寄せ、投稿担体と read view のズレ（f3）を消す。

## 受け入れ基準

- FakeForge で `add_pr_label` を 1 回失敗させても、対象 PR の escalation コメントが高々 1 件
  であること（sweep を複数回回す単体テスト）。
- ラベルが付いた状態で観測が stale（`human_stop = false`）でも、同一 head×reason では
  2 件目のコメントも 2 通目の通知も出ないこと。
- ラベル追加失敗の sweep 後、`working` が PR に残っていないこと（次 sweep の再試行が
  `pr_is_touchable` に塞がれない、f1）。
- `escalate_pr` が `Deferred` / `Deduped` を返したとき、caller の固有イベント
  （`reconciler.budget_exhausted` 等）が emit されないこと（f4）。
- `GhForge::pr_comments_meta` の GraphQL parse で `viewer_did_author` が
  真/偽とも正しく埋まること（f2 の単体テスト）。
- 第三者（`viewer_did_author == false`）が偽マーカーコメントを置いても escalation が
  抑止されないこと。
- 新しい head を push した後は、同一 reason でも再エスカレーション（コメント 1 件）できること。
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run` /
  `cargo test --doc` が通ること。
- **同時実行の限界**（f5）: 「高々 1 件」は単一 instance の直列化された escalation 前提での
  保証であり、クロス instance 同時発火は二重投稿しうる（冪等ラベルで needs-human へ収束）。
  この前提が崩れる構成では保証しない、と明記する（受け入れテストの対象外）。
