# ADR 0027: claim identity と no-steal — 家族横断 active-run インデックス + instance マーカー射影

- Status: accepted
- Date: 2026-07-21
- Issue: #223(ADR 0012 スライス3 に合流。旧 #201)

## Context

これまで fixer 家族(fixer / ci_fixer / conflict_resolver)の「この PR は今 誰かが処理中」は
`meguri:working` ラベルで表していた。ここに 3 つの問題がある。

1. **家族を横断しない排他**。現行の active-run ユニークインデックスは
   `(project_id, loop_kind, issue_number)` 単位(`0007_tasks.sql`)なので、Fixer 実行中に CI が
   赤へ変わると CiFixer は別 `loop_kind` として enqueue できてしまう。「PR に fixer 家族は同時 1 本」を
   ラベルの discovery-time skip だけに頼っていた。
2. **claim に owner が無い**。ラベルは付いているかどうかしか表せず、「どの instance が握っているか」を
   載せられない。複数ホスト(Phase-4)へ進めない。
3. **claim を弱い権限の担体へ移す誘惑**。「claim の真実を PR コメントにする」だけだと、公開リポジトリでは
   コメントは誰でも書けるので、第三者が偽の claim を投稿して自動処理を凍結できてしまう。

## Decision

**排他の権威は sqlite の家族横断 active-run 部分ユニークインデックスに置き、instance 名入りマーカーを
その forge 射影とする。`meguri:working` は人間向けの表示射影へ格下げする。**

### 家族横断インデックス(単一 instance の権威)

migration 0016 で追加:

```sql
CREATE UNIQUE INDEX runs_active_fixer_family
  ON runs(project_id, issue_number)
  WHERE loop_kind IN ('conflict-resolver','ci-fixer','fixer')
    AND status IN ('queued','running','interrupted') AND issue_number IS NOT NULL;
```

これで 1 PR(canonical issue)に fixer 家族の active run は最大 1 本。atomic な排他はここで効く。
upgrade 時に既存の重複(Fixer + CiFixer 同時 active など)があると `CREATE UNIQUE INDEX` が失敗して
store 起動が止まるので、**同 migration の中で index を張る前に、各群を最新の 1 本(`created_at DESC` →
`started_at DESC` → `id`)へ畳み、残りを `status='cancelled'`(`succeeded` ではないので予算不変)へ
terminal 化する**前進クリーンアップを先に流す。

### instance マーカー(forge 射影 + クロスホスト前準備)

claim を PR コメント `<!-- meguri:claim instance=<id> run=<run_id> -->` として投稿する
(arm 非依存・head 非依存)。**信頼するのは `viewerDidAuthor == true`(meguri 自身が投稿)のマーカー
だけ** —— 第三者の偽マーカーは無視され、偽装で no-steal を凍結できない。排他判定はマーカーの存在では
なく **`run_id` を runs 表で引いた生存**で決める:

- run が **active** → skip(no-steal / 家族排他)。
- run が **terminal / 見つからない** → マーカーは stale → 無視して reclaim。

release は run 終端時にマーカーを node `id` で tombstone 編集する best-effort だが、**correctness は
tombstone の成否に依存しない** —— 生存判定が本線なので、編集失敗・instance 名変更・別 instance でも
永久停止しない。instance id は `[reconciler] instance`(既定 = `mux.session`)。

## reconciler の busy gate は run-liveness

reconciler が「この issue は今 誰かが処理中か」を判定する gate も、`meguri:working` **ラベルではなく
run の生存**で決める(author lane の live run が 1 本でもあれば PR に触らない。`pr-reviewer` は別 lane・
detached worktree なので除外)。ラベルで gate すると、クラッシュした run が残した stale な `working` が
永久に PR をブロックし、fixer arm だけでなく budget escalation・stuck backstop まで止まって自動復旧
できなくなる。run が terminal / 消失なら not-busy と読むので、クラッシュ後も次 resync で自然に arm 可能へ
戻る。`meguri:working` は付け外しを続ける表示射影にすぎない。

## Consequences

- 単一 instance では家族横断インデックスが実効の排他。マーカーはその forge 射影で、Phase-4 の共有 DB で
  クロスホスト権威に昇格する(その時も自著判定は残り、`instance` 欄が並行 meguri を区別する)。
- `meguri:working` は付け外しを続けるが権威ではない(表示射影)。rollback しても旧コードは working ラベルで
  直列化するので二重 claim は起きない。残置する家族横断インデックスは旧コードが依存も違反もしない。

## backoff の永続(ADR 0012 決定6 の精緻化)

同スライスで導入した `reconciler_backoff`(fixer ping-pong の指数バックオフ)について、ADR 0012 決定2/6 を
一段精緻化する: **workqueue のうち sqlite へ永続するのは backoff の `next_visible_at` /
`scheduled_attempt` / `baseline_attempt` だけ**。activeQ / parked は `Wait` verdict と forge ラベルから
毎 resync 再導出できる(第2の権威を作らない)。backoff は症状 episode 単位でリセットする —— 指数は
`n(=succeeded_run_count) - baseline_attempt` で、positive 解決(緑 / mergeable / スレッド無し)で行を
消すと次の症状は新 episode として 0 から数え直す。全期間カウントの `succeeded_run_count` を指数に直用
すると再発時に過去 episode の待ちを引き継ぐ問題を、`baseline_attempt` の高水位マークで断つ。
