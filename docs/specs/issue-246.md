# issue #246 — escalation の冪等化: read-after-write 検証 + escalation comment の dedup

> 使い捨ての足場（`docs/adr/0001-specs-are-disposable-scaffolding.md`）。
> 恒久的な設計判断は ADR 0028 に切り出し済み。実装が landしたらこの spec は削除する。

## ゴール

level-triggered なループが同一エスカレーションを重複発火させないようにする。PR #231 に
同一文面の needs-human コメントが3回付いた故障（`docs/design/needs-human-friction-and-delivery-speed.md`
§3-C）を、ラベル書き込みの read-after-write と、コメントを head-keyed marker にした dedup で塞ぐ。

## spec の深さ — normal（理由）

永続ステート・スキーマ・公開契約のいずれにも触れない（DB マイグレーション不要）。変更は
オーケストレータ内の forge 書き込み順序と、コメントに埋める hidden marker の追加に閉じる。
一方で blast radius は「PR エスカレーションの唯一の choke point」= 8 caller に及ぶため、
決定事項（下記）を先に固め、代替案は ADR 0028 に記録した。よって **normal spec** とする。

## 受け入れ基準

- FakeForge で `add_pr_label` を1回失敗させても、エスカレーションコメントが**高々1件**である
  （1回目: ラベル失敗 → コメント無し・イベント無し / 2回目 sweep: ラベル成功 → コメント1件）。
- ラベル書き込みが成功したのち stale 読みで discovery が再発火しても、現 head / 同一 reason の
  marker があればコメントは**再投稿されない**（2回目以降は dedup で skip）。
- 別種の理由（budget 枯渇 vs stuck vs review 失敗）は同じ head でも**それぞれ1回**出せる。
- 既存テストが緑のまま（`spec_fixer` の budget 枯渇テストのコメント数・イベント数・通知数の
  期待は不変 = 各1件）。

## 主要な決定（すべてこの pass で確定）

1. **どこに実装するか**: PR エスカレーションの choke point `escalation::escalate_pr` に両機構を
   実装する。そこを通る全 caller が恩恵を受ける。加えて、`escalate_pr` を通らず label+comment を
   手書きしていた `spec_fixer::escalate_budget_exhausted`（issue が名指しした故障箇所）は、
   共有の write primitive を使って同じ2機構を得る。

2. **write primitive を切り出す**: `escalation.rs` に「ラベル read-after-write + head-keyed
   dedup + working 解除 + comment 投稿」だけを行い、**イベント/通知は出さない** primitive を置く:

   ```rust
   pub enum Escalated { Posted, Deferred, Deduped }

   /// label を立てられなければ Deferred（何も出さず次 sweep に委ねる）。
   /// head=Some で marker が既在なら Deduped（再投稿しない）。
   /// それ以外は working を外し comment(+marker) を投稿して Posted。
   async fn park_pr(
       deps: &Deps, pr: i64, head_sha: Option<&str>, reason: &str, comment: &str,
   ) -> Escalated;
   ```

   `escalate_pr` は `park_pr` を呼び、戻り値が `Posted` のときだけ `escalation.raised` を emit し
   `escalation_pr` を notify する。`spec_fixer::escalate_budget_exhausted` も `park_pr` を呼び、
   `Posted` のときだけ既存の `spec_fixer.budget_exhausted` emit と `awaiting_human` page を出す
   （escalation_pr の二重通知は起こさない → 既存テストの `delivered.len()==1` を維持）。

3. **`escalate_pr` のシグネチャ**: `escalate_pr(deps, pr, head_sha: Option<&str>, reason: &str,
   comment: &str)`。
   - `PullRequest` を持つ level-triggered サイト（reconciler の budget/stuck/review-failed、
     pr-reviewer guard の `cp.head_sha`、spec_fixer budget の `pr.head_sha`）は `Some(head)` を渡す。
   - turn 完了時の flavor `escalate`（ci-fixer / fixer / conflict-resolver / spec-worker /
     pr-reviewer の `escalate_on_pr`）は head を安価に持たないため `None` を渡す。read-after-write
     のみ効き、comment dedup は skip する（これらは1ターン1回発火で level-triggered ではないため
     read-after-write だけで二重発火は防げる）。

4. **marker 形式**（ADR 0003 の arm marker と同型、`escalation.rs` に helper）:
   ```
   <!-- meguri:escalated head=<sha> reason=<key> -->
   ```
   - `escalation_marker_prefix()` と `escalation_marker(head_sha, reason)` を定義。
   - `park_pr` は comment 末尾に marker を追記して投稿する。`head=None` のときは marker を付けず
     dedup もしない。

5. **reason キー**（marker の弁別子、各サイトで literal 定数を渡す）:
   `spec-fix-budget` / `ci-budget` / `conflict-budget`（reconciler）/ `stuck` /
   `pr-review-failed`。flavor `escalate`（head=None）は marker を書かないので reason は無視される。

6. **順序と失敗セマンティクス**（`park_pr` 内）:
   1. `add_pr_label(needs-human)` — `Err` なら `Deferred` を返す（comment・working 解除・
      イベント・通知いずれも無し）。
   2. `head=Some(h)` なら `pr_comments(pr)` を live read し、`escalation_marker(h, reason)` を
      含むコメントがあれば `Deduped` を返す（再投稿しない）。**read 自体が失敗したら投稿へ進む**
      — 稀な read 障害で高々1件重複する方が、エスカレーションを握りつぶすより安全（ラベルは
      既に read-after-write で保証済み）。
   3. `remove_pr_label(working)`（best-effort, `let _`）。
   4. `pr_comment(comment + "\n\n" + marker)` を投稿し `Posted` を返す。

7. **reconciler の付随イベントを gate する**: `escalate_stuck` / `escalate_pr_review_failed` /
   `escalate_budget_exhausted`（reconciler）は現在 `escalate_pr` の後で自分のイベントを無条件に
   emit している。`escalate_pr` の戻り値が `Posted` のときだけ emit するよう変更し、
   deferred/deduped なエスカレーションが指標を水増ししないようにする。

8. **`escalate_issue`（issue-native）**: `add_label` に read-after-write を適用する（失敗したら
   comment・event・notify を出さず次 sweep に委ねる）。head を持たないため comment marker dedup は
   対象外（本 issue は PR エスカレーションの再発を扱う）。

9. **FakeForge のテスト支援**: ラベル書き込みを故意に失敗させる仕組みが無い。
   `add_pr_label` が指定 PR で N 回失敗してから成功する `fail_pr_label(pr, count)` を追加する
   （既存の `fail_comment` / `fail_merge_state` と同じ Mutex フィールド様式。一度きりの失敗を
   数える `Mutex<HashMap<i64, u32>>`）。

## 触るファイル

- `src/engine/escalation.rs` — `park_pr` / `Escalated` / `escalation_marker*` を追加。
  `escalate_pr` を `park_pr` ベースに書き換え（シグネチャに `head_sha` と `reason` を追加）。
  `escalate_issue` に read-after-write を追加。
- `src/engine/spec_fixer.rs` — `escalate_budget_exhausted` を `park_pr` 経由にし、
  付随 emit/notify を `Posted` で gate。
- `src/engine/issue_reconciler.rs` — `escalate_budget_exhausted` / `escalate_stuck` /
  `escalate_pr_review_failed` の `escalate_pr` 呼び出しに `Some(&pr.head_sha)` と reason を渡し、
  付随 emit を `Posted` で gate。
- `src/engine/ci_fixer.rs` / `fixer.rs` / `conflict_resolver.rs` / `spec_worker.rs` /
  `pr_reviewer.rs` — `escalate_pr` 呼び出しを新シグネチャ（flavor は `None`、pr-reviewer guard は
  `Some(&cp.head_sha)`）に更新。
- `src/forge/fake.rs` — `fail_pr_label(pr, count)` フィールド + 判定を追加。

## テスト戦略

- `escalation.rs` 単体: `fail_pr_label(pr, 1)` で `park_pr` を2回呼び、コメントが1件・
  1回目は `Deferred`・2回目は `Posted` を確認（受け入れ基準の主軸）。
- `escalation.rs` 単体: 同一 head/reason で `park_pr` を2回 → 2回目は `Deduped`・コメント1件。
  別 reason では2件出ることも確認。
- `spec_fixer` 既存テスト（`discover_escalates_when_the_fix_budget_is_spent`）が緑のまま
  （コメント1件・`spec_fixer.budget_exhausted` 1件・`awaiting_human` page 1件）を維持。
  加えて、ラベルを1回失敗させると discovery 2 sweep でコメントが1件に収まる回帰テストを足す。
- `cargo fmt --check` / `clippy -D warnings` / `nextest` / `test --doc` を通す。

## 非対象

- issue-native の comment dedup（head を持たないため）。
- P1/P3/P4 など design doc の他改善（別 issue）。
- 書き込みのリトライ機構（level-triggered の「次 sweep で再試行」で足りる、ADR 0028 参照）。
