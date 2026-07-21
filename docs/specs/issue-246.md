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
- ラベル書き込みが**失敗した経路でも `working` claim は外れる**ので、次 sweep が同じ PR/issue を
  再発見して再試行できる（`pr_is_touchable` は `working` を `needs-human` より先に見るため、
  claim が残ると再試行が止まる — f1/f2）。
- ラベル書き込みが成功したのち stale 読みで discovery が再発火しても、現 head / 同一 reason の
  **自著**の marker があればコメントは**再投稿されない**（2回目以降は dedup で skip）。
- 第三者が公開 head/reason の marker を**先に投稿**しても、自著（`viewer_did_author`）でないため
  無視され、最初の正当なエスカレーションは `Posted`（event・human paging も抑止されない — f4）。
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

   /// label を立てられなければ working を外して Deferred（次 sweep に委ねる）。
   /// head=Some で自著の marker が既在なら working を外して Deduped（再投稿しない）。
   /// それ以外は working を外し comment(+marker) を投稿して Posted。
   /// いずれの経路でも working claim は解放する（f1）。
   async fn park_pr(
       deps: &Deps, pr: i64, head_sha: Option<&str>, reason: &str, comment: &str,
   ) -> Escalated;
   ```

   `escalate_pr` は `park_pr` を呼び、戻り値が `Posted` のときだけ `escalation.raised` を emit し
   `escalation_pr` を notify する。**`escalate_pr` 自身も `Escalated` を返す**ので、caller は
   直後の付随イベントを gate できる（item 7）。`spec_fixer::escalate_budget_exhausted` も
   `park_pr` を呼び、`Posted` のときだけ既存の `spec_fixer.budget_exhausted` emit と
   `awaiting_human` page を出す（escalation_pr の二重通知は起こさない → 既存テストの
   `delivered.len()==1` を維持）。

3. **`escalate_pr` のシグネチャ**: `escalate_pr(deps, pr, head_sha: Option<&str>, reason: &str,
   comment: &str) -> Escalated`。
   - `PullRequest` を持つ level-triggered サイト（reconciler の budget/stuck/review-failed、
     pr-reviewer settle の impl blocking guard は `cp.head_sha`、spec_fixer budget は
     `pr.head_sha`）は `Some(head)` を渡す。これらは毎 sweep 再評価される＝重複発火の本体なので
     dedup が要る。
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

5. **reason キー**（marker の弁別子、各サイトで literal 定数を渡す。同じ head でも別 reason は
   それぞれ1回出せる）:
   `spec-fix-budget` / `ci-budget` / `conflict-budget`（reconciler の budget arm）/ `stuck` /
   `pr-review-failed`（reconciler auto-merge）/ **`pr-review-guard`（pr-reviewer の impl blocking
   guard — f3）**。flavor `escalate`（head=None）は marker を書かないので reason は無視される。

6. **順序と失敗セマンティクス**（`park_pr` 内、**全経路で `working` を解放する**）:
   claim が残ると `pr_is_touchable` が `needs-human` より先に `working` を見て次 sweep を止めて
   しまう（f1）。エスカレーション成立時は「needs-human を付けてから working を外す」順にして、
   PR が「未 claim かつ未エスカレーション」に見える瞬間を作らない。
   1. `add_pr_label(needs-human)` を試す。`Err` の場合: `remove_pr_label(working)`（best-effort）
      してから `Deferred` を返す（comment・event・notify なし）。needs-human は付いていないので
      次 sweep が再発見して再試行する。← defer 経路の claim 解放（f1）
   2. `head=Some(h)` の場合、**`pr_comments_meta(pr)` を live read** し、**自著
      （`viewer_did_author`）** かつ `escalation_marker(h, reason)` を含むコメントがあれば、
      `remove_pr_label(working)` してから `Deduped` を返す（再投稿しない）。第三者が同じ marker を
      先に投稿しても自著判定で無視する（f4、ADR 0027 の claim marker と同じ立て付け）。read 自体が
      失敗したら投稿へ進む — 稀な read 障害で高々1件重複する方が握りつぶすより安全（ラベルは
      read-after-write 済み）。← dedup 経路の claim 解放（f1）＋ 自著判定（f4）
   3. `remove_pr_label(working)`（best-effort, `let _`）。
   4. **`comment_pr`**（`pr_comment` ではない）で `comment + "\n\n" + marker` を投稿し `Posted` を
      返す。FakeForge では `comment_pr` が dedup の read 元（`PrComment` 配列）に自著コメントとして
      記録し、read/write 経路が一致する（f5）。実 forge ではどちらも同じ会話コメントで差はない。

7. **付随イベントを `Posted` で gate する**: `escalate_pr` 直後に無条件 emit している付随イベントを
   `Escalated == Posted` のときだけ出すよう変更し、deferred/deduped が指標を水増ししないように
   する（f3）:
   - reconciler: `reconciler.budget_exhausted` / `pr.merge_watch_stuck` / `automerge.pr_review_failed`
   - pr-reviewer settle（impl blocking guard）: `pr_review.escalated`

8. **`escalate_issue`（issue-native）**: `add_label(needs-human)` に read-after-write を適用する。
   `Err` の場合は **`remove_label(working)`（best-effort）してから return** し、comment・event・
   notify を出さない — PR 側と同じく claim を解放して次 sweep が同じ issue を拾えるようにする
   （f2）。成功経路は従来どおり「needs-human を付けてから working を外す」順。head を持たないため
   comment marker dedup は対象外（本 issue は PR エスカレーションの再発を扱う）。

9. **FakeForge のテスト支援**（2つ追加）:
   - `fail_pr_label(pr, count)`: `add_pr_label` が指定 PR で N 回失敗してから成功する（既存の
     `fail_comment` / `fail_merge_state` と同じ Mutex フィールド様式。失敗回数を数える
     `Mutex<HashMap<i64, u32>>`）。ラベル書き込み失敗の注入に使う。
   - `push_pr_comment_from_other(pr, body)`: `viewer_did_author = false` の `PrComment` を
     `pr_comments` 配列へ直接積む（第三者が投稿した marker を再現）。f4 の自著判定テストに使う
     （`comment_pr` は常に自著なので、非自著コメントを別ルートで seed する必要がある）。

## 触るファイル

- `src/engine/escalation.rs` — `park_pr` / `Escalated` / `escalation_marker*` を追加。
  `escalate_pr` を `park_pr` ベースに書き換え（シグネチャに `head_sha` と `reason` を追加し、
  `Escalated` を返す）。dedup の read は `pr_comments_meta` + `viewer_did_author` フィルタ、
  投稿は `comment_pr`。`escalate_issue` に read-after-write（失敗時は `working` 解放）を追加。
- `src/engine/spec_fixer.rs` — `escalate_budget_exhausted` を `park_pr` 経由にし、
  付随 emit/notify を `Posted` で gate。
- `src/engine/issue_reconciler.rs` — `escalate_budget_exhausted` / `escalate_stuck` /
  `escalate_pr_review_failed` の `escalate_pr` 呼び出しに `Some(&pr.head_sha)` と reason を渡し、
  付随 emit（`reconciler.budget_exhausted` / `pr.merge_watch_stuck` / `automerge.pr_review_failed`）
  を `Posted` で gate。
- `src/engine/pr_reviewer.rs` — impl blocking guard の `escalate_pr` に `Some(&cp.head_sha)` +
  reason `pr-review-guard` を渡し、直後の `pr_review.escalated` emit を `Posted` で gate（f3）。
  `escalate_on_pr`（flavor）は `None`。
- `src/engine/ci_fixer.rs` / `fixer.rs` / `conflict_resolver.rs` / `spec_worker.rs` —
  flavor `escalate` の `escalate_pr` 呼び出しを新シグネチャ（`None` + reason）に更新。
- `src/forge/fake.rs` — `fail_pr_label(pr, count)` と `push_pr_comment_from_other(pr, body)` を追加。

## テスト戦略

- `escalation.rs` 単体: `fail_pr_label(pr, 1)` で `park_pr` を2回呼び、コメントが1件・
  1回目は `Deferred`・2回目は `Posted` を確認（受け入れ基準の主軸）。加えて `Deferred` 後に
  `working` が外れていることを確認（f1）。
- `escalation.rs` 単体: 同一 head/reason で `park_pr` を2回 → 2回目は `Deduped`・コメント1件、
  かつ `working` は解放済み。別 reason では2件出ることも確認。
- `escalation.rs` 単体（f4）: `push_pr_comment_from_other` で非自著の marker を先に置く →
  `park_pr` は `Deduped` にならず `Posted`（自著コメントが1件増える）。
- `escalation.rs` 単体（f2）: `escalate_issue` で issue の `add_label` を1回失敗させ、
  `working` が外れ・comment/event/notify なし → 次呼び出しで成功しコメント1件。
- `spec_fixer` 既存テスト（`discover_escalates_when_the_fix_budget_is_spent`）が緑のまま
  （コメント1件・`spec_fixer.budget_exhausted` 1件・`awaiting_human` page 1件）を維持。
  加えて、ラベルを1回失敗させると discovery 2 sweep でコメントが1件に収まる回帰テストを足す。
- `cargo fmt --check` / `clippy -D warnings` / `nextest` / `test --doc` を通す。

## 非対象

- issue-native の comment dedup（head を持たないため。read-after-write と claim 解放は適用）。
- P1/P3/P4 など design doc の他改善（別 issue）。
- 書き込みのリトライ機構（level-triggered の「次 sweep で再試行」で足りる、ADR 0028 参照）。
