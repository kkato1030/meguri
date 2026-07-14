# ADR 0009 — parked review を人間へ能動シグナル化する(interaction_state + notify、会話タイムライン外)

- Status: Accepted
- Date: 2026-07-13
- Issue: #153(#83 から継承した1論点)
- 関連: ADR 0008(`0008-symmetric-plan-impl-review-loop.md`、#132、main 着地済み — **番号が `0008-agent-instructions-via-apm.md` と重複しているため、本文書の「ADR 0008」は常に前者・対称化 ADR を指す**)、ADR 0007(merge-watch は fixer 系に委譲)、ADR 0005(ラベル二軸)、ADR 0013(spec_fixer — findings park の自動修正ループ。本 ADR は能動シグナル側で補完関係)、ADR 0014(plan findings は spec_fixer に委譲しエスカレートしない)

## Context

`InteractionState::AwaitingHuman` は現状「**走行中の turn** が人間の入力を求めた」瞬間に立つ **run スコープ**の状態である(`src/turn/mod.rs` の `AgentState::Blocked` / nudge 上限 / runtime 上限)。通知は turn ループの中で `StoreControl::event("turn.awaiting_human", …)` が `deps.notifier.notify_awaiting_human` を叩いて配送し、dashboard(`meguri top`)は `interaction_state == AwaitingHuman` の行を `▶` で強調する(`src/app.rs`)。

一方 spec レビュー(pr_reviewer(Plan)、`src/engine/pr_reviewer.rs` — ADR 0008 の対称化で旧 `spec_reviewer` を置換し、#190 で guard → pr-reviewer に rename 済み)には、**run が正常終了した後**に人間の判断を待つ park が存在する:

- **findings park** — pr_reviewer(Plan) が findings を返すと `settle` は `meguri/pr-review` の commit status(failure)を書き、レビューを PR 本文の `<details>` に折り込んで `meguri:spec-reviewing` を維持したまま run を `Succeeded` で終える(`src/engine/pr_reviewer.rs:665-709`)。同じ head は二度レビューされない(commit status が dedup key、同 `:207-215`)ため、次の push があるまで PR は静かに固まる。
- **clean park** — `plan_delivery=separate`(既定、`src/config.rs:832-843`)では clean で `spec-reviewing → spec-ready` に遷移した後、spec PR のマージは人間の仕事になる(`spec_worker` は combined でしか discover しない — `src/engine/spec_worker.rs:66`。auto-merge も spec ラベルの付いた PR は arm せず(`src/engine/auto_merger.rs` の `BLOCKING_LABELS`)、handoff sweep(`src/engine/plan_handoff.rs`)が issue を `ready` に進めるのも**マージ後**)。combined では `spec_worker` が自動継続するため park ではない。

ADR 0008 は pr_reviewer findings を `pr-review` の commit status + PR 本文 `<details>` として**可視化**するが、**能動通知経路は持たない**(ADR 0008 の needs-human 昇格は auto-merge の pr_reviewer(Impl) failure に限られる)。結果、parked spec レビューには「人間に届く能動シグナル」が無い。

この park は turn スコープではなく**ワークフロー階層の待ち**であり、既存の run スコープ `AwaitingHuman` とは噛み合っていない。

なお findings park を「まず自動で直す」ループは spec_fixer(#188 / ADR 0013、**着地済み**)が担う。役割分担は spec_fixer = 自動修正側、本 ADR/#153 = 能動シグナル側。spec_fixer はラウンド上限まで findings を自動修正し、上限を使い切ってなお red なら `needs-human` へ park する — その park を PR を指す awaiting_human で能動通知するのが本 ADR の担当(`escalate_budget_exhausted`)。よって findings-park の page は「毎回」ではなく「上限超過時の1回」。本 ADR が守る不変条件は「**無標識の park を作らない**」ことである。

## Decision

pr_reviewer(Plan) が park する終端で、既存の `InteractionState` + `Notifier` レールを **ワークフロー階層の待ちにも流用**する。具体的には park 時に:

1. run は `Succeeded` のまま終える。**park 専用の `RunStatus` は増やさない。**
2. 終える直前に、その run の `interaction_state` を `AwaitingHuman` にセットする(`update_run_status(Succeeded)` は `interaction_state` を触らないので、Succeeded 化の後も残る)。
3. `deps.notifier.notify_awaiting_human(…)` を**エンジンから直接** imperative に叩く。通知配送は run の status に一切依存しない(turn の外なので `StoreControl` は経由しない)。通知の遷移先は pane ではなく PR なので、`Notification` の pane 前提の契約(`attach` = pane に attach する shell command)は流用せず、任意の `url` フィールドを新設して PR URL を載せる(`attach` の Option 化・表示文言・webhook の `attach_cli` の扱いは spec に記載)。
4. `review.awaiting_human` イベントを emit する(turn スコープの `turn.awaiting_human` と event ストリーム上で区別する)。

**同一 head への page は best-effort とする。** pr-reviewer の run は head ごとに新規に作られる(discovery は head に `meguri/pr-review` status がまだ無い PR だけを拾う — status が dedup key)ため、`Notifier` の per-`run_id` throttle は通常運転では head 単位の1回配送に一致する。ただしこの throttle は**プロセス内メモリの時間窓**による重複抑止であり(`src/notify/mod.rs` — 配送記録は永続化されず、窓を過ぎれば同一 run_id でも再配送される)、「head につき厳密に1回」の保証ではない。`settle` は中断・再開で同じ run を再実行し得るし、デーモン再起動で throttle 状態は失われる。これは意図的に許容する: 通知チャネル自体が best-effort(失敗はログのみで turn を落とさない)であり、重複 page は人間に同じ待ち状態を再提示するだけで無害。欠落 page こそが本 ADR が直す欠陥である。厳密冪等の永続マーカーは代替案 D として却下した。

**dashboard 強調は小改修を要する**(本 ADR の唯一の実測リスク検証結果)。`meguri top` の `top_refresh` は `store.list_runs(true)`(= `queued`/`running`/`interrupted` のみ)を読むため、`Succeeded` で park した run は行に現れず、`▶` 強調も出ない。`list_runs(true)` は scheduler も駆動対象の抽出に使う共有関数なので**意味を変えてはならない**。よって review park だけを拾う専用クエリを追加し、dashboard 側だけがそれを強調行として合流させる(pane が無くても表示する — 動作対象は pane ではなく PR)。ここで拾う印は**状態(`AwaitingHuman`)ではなく `review.awaiting_human` イベントの存在**でなければならない。`AwaitingHuman` は turn 側でも立ち(`turn.awaiting_human`)、`update_run_status` はそれを消さないので、(1) 人間待ちのまま stop/cancel/failed/skipped で終わった run、(2) pr_reviewer 自身の turn(Impl や combined-Plan)が runtime/quiet で awaiting_human になった後に同じ run が `Succeeded` で終わった場合、のどちらも状態だけでは釣れてしまう。この2つは park ヘルパを**呼ばない**経路である。park ヘルパが実際に走ったことを表すのは、ヘルパが emit する `review.awaiting_human` イベントだけ。よってクエリは `status='succeeded'`(異常終了の保険)+ `interaction_state='awaiting_human'`(未クリアの担保)+ **その run に `review.awaiting_human` イベントがある**ことを条件にする。`loop_kind` では上記 (2) を除けない。

**通知手段はコメント投下ではない。** park は `interaction_state` + notify(会話タイムライン外)で表現し、PR 会話へは何も足さない。これは ADR 0008 の「検査履歴は会話タイムライン外(status + `<details>`)」原則と衝突しない。

## Rationale / 却下した代替案

- **(A) park 専用 `RunStatus` を新設** — scheduler・dashboard・daemon・cleaner など `RunStatus` を分岐する全経路に波及する。「run は Succeeded で終わった正常な workflow」という事実とも噛み合わない。過剰。却下。
- **(B) `meguri:needs-human` ラベルを貼る** — ADR 0008 の needs-human 昇格は auto-merge の pr_reviewer(Impl) **failure** に限定されている。findings は失敗ではなく**正常な人間ゲート**であり、ball 軸(ADR 0005)の needs-human とは意味が違う。加えて `needs-human` はループの締め出し(ADR 0007 のデッドロック注意)を招きうる — #188 の spec_fixer は `needs-human` の付いた PR を discover しないので、自動修正の芽を摘んでしまう。却下。
- **(C) PR に「レビュー待ちです」コメントを投下** — 会話タイムラインを汚し、ADR 0008 の会話タイムライン外原則に反する。却下。
- **(D) 永続マーカーで head 単位の厳密1回配送を保証** — notify 前に当該 run の `review.awaiting_human` イベント有無(または専用マーカー)を照会すれば、再実行・再起動をまたぐ重複を防げる。しかし emit と配送は原子的でない(先に emit すれば crash で通知欠落、先に配送すれば重複)ため厳密化しても保証は閉じず、best-effort チャネルの上に冪等機構を積むことになる。重複の害(同じ page がもう1回届く)< 欠落の害(park が無標識に戻る)。過剰。却下。
- **採用(interaction_state + notify)** — 既存レールの最小拡張で、run スコープの意味を workflow スコープへ広げるだけ。通知の重複抑止は既存 throttle にそのまま乗る(best-effort — Decision 参照)。

## Consequences

- `InteractionState::AwaitingHuman` の意味が「走行中 turn の待ち」から「run が終わった後の workflow 待ち」へ拡張される。両者は event 名(`turn.awaiting_human` / `review.awaiting_human`)で区別する。
- **parked な `interaction_state` のクリアが必要**になる。放置すると Succeeded run の `AwaitingHuman` が DB に残り、dashboard に古い park が滞留する。次 head の review 着手時(同一 issue の prior park を解消)および issue close 時(reaper)にクリアする。
- park シグナルのヘルパは kind 非依存に作る。`pr_reviewer.rs` の settle は **clean-park のみ**(`plan_delivery=separate` のとき)これを呼ぶ。findings-park の page は spec_fixer(ADR 0013、着地済み)のラウンド上限超過エスカレーション(`escalate_budget_exhausted`)へ移した — そこは turn の外なので run を持たず、PR を指す run-less な page(`interaction_state` は付けない。dashboard 行は spec_fixer が貼る `needs-human` ラベルが担う)。ADR 0013 が「上限超過や clean-park の awaiting_human 通知は #153 の担当」と明記しているのと対応する。
- 当初は findings 直後にも page する暫定案だったが、spec_fixer(ADR 0013)が着地して findings を自動修正するようになったため、findings の page は「毎回」から「ラウンド上限超過時の1回」へ移った。本 ADR のシグナル機構(interaction_state + notify + event、および notify の PR 指し)自体は変わらない。
- `docs/architecture/loops.md` の ADR 索引(§5)への本 ADR の追加は、doc 全面追随(issue #172、同 doc §6 に予告済み)に合流させる — 本 ADR 単独では触らない。
