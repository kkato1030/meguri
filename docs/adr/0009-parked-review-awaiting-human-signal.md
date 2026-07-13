# ADR 0009 — parked review を人間へ能動シグナル化する(interaction_state + notify、会話タイムライン外)

- Status: Accepted
- Date: 2026-07-13
- Issue: #153(#83 から継承した1論点)
- 関連: ADR 0008(symmetric-plan-impl-review-loop、in-flight #132)、ADR 0007(merge-watch は fixer 系に委譲)、ADR 0005(ラベル二軸)

## Context

`InteractionState::AwaitingHuman` は現状「**走行中の turn** が人間の入力を求めた」瞬間に立つ **run スコープ**の状態である(`src/turn/mod.rs` の `AgentState::Blocked` / nudge 上限 / runtime 上限)。通知は turn ループの中で `StoreControl::event("turn.awaiting_human", …)` が `deps.notifier.notify_awaiting_human` を叩いて配送し、dashboard(`meguri top`)は `interaction_state == AwaitingHuman` の行を `▶` で強調する(`src/app.rs`)。

一方 spec レビューには、**run が正常終了した後**に人間の判断を待つ park が存在する:

- **findings park** — `spec_reviewer` が findings を返すと `settle` は PR コメントを1回投下して `meguri:spec-reviewing` を維持したまま run を `Succeeded` で終える(`src/engine/spec_reviewer.rs`)。同じ head は二度レビューされない(head-sha マーカーの冪等性)ため、人間が fix を push するまで PR は静かに固まる。
- **clean 手前の gate**(#132 / ADR 0008 で `plan_delivery=separate` 既定 = 人間が spec PR をマージ、が着地した後)— clean でも「人間が spec PR をマージするのを待つ」park になる。

ADR 0008 は guard findings を `guard-review` の commit status + PR 本文 `<details>` として**可視化**するが、**能動通知経路は持たない**(ADR 0008 の needs-human 昇格は auto-merge guard(Impl) failure に限られる)。結果、parked spec レビューには「人間に届く能動シグナル」が無い。

この park は turn スコープではなく**ワークフロー階層の待ち**であり、既存の run スコープ `AwaitingHuman` とは噛み合っていない。

## Decision

reviewer/guard が park する終端で、既存の `InteractionState` + `Notifier` レールを **ワークフロー階層の待ちにも流用**する。具体的には park 時に:

1. run は `Succeeded` のまま終える。**park 専用の `RunStatus` は増やさない。**
2. 終える直前に、その run の `interaction_state` を `AwaitingHuman` にセットする(`update_run_status(Succeeded)` は `interaction_state` を触らないので、Succeeded 化の後も残る)。
3. `deps.notifier.notify_awaiting_human(…)` を**エンジンから直接** imperative に叩く。通知配送は run の status に一切依存しない(turn の外なので `StoreControl` は経由しない)。通知の遷移先は pane ではなく PR なので、`Notification` の pane 前提の契約(`attach` = pane に attach する shell command)は流用せず、任意の `url` フィールドを新設して PR URL を載せる(`attach` の Option 化・表示文言・webhook の `attach_cli` の扱いは spec に記載)。
4. `review.awaiting_human` イベントを emit する(turn スコープの `turn.awaiting_human` と event ストリーム上で区別する)。

**同一 head への page は best-effort とする。** reviewer/guard の run は head ごとに新規に作られる(discovery は head-sha マーカー未付与の PR だけを拾う)ため、`Notifier` の per-`run_id` throttle は通常運転では head 単位の1回配送に一致する。ただしこの throttle は**プロセス内メモリの時間窓**による重複抑止であり(`src/notify/mod.rs` — 配送記録は永続化されず、窓を過ぎれば同一 run_id でも再配送される)、「head につき厳密に1回」の保証ではない。`settle` は中断・再開で同じ run を再実行し得るし、デーモン再起動で throttle 状態は失われる。これは意図的に許容する: 通知チャネル自体が best-effort(失敗はログのみで turn を落とさない)であり、重複 page は人間に同じ待ち状態を再提示するだけで無害。欠落 page こそが本 ADR が直す欠陥である。厳密冪等の永続マーカーは代替案 D として却下した。

**dashboard 強調は小改修を要する**(本 ADR の唯一の実測リスク検証結果)。`meguri top` の `top_refresh` は `store.list_runs(true)`(= `queued`/`running`/`interrupted` のみ)を読むため、`Succeeded` で park した run は行に現れず、`▶` 強調も出ない。`list_runs(true)` は scheduler も駆動対象の抽出に使う共有関数なので**意味を変えてはならない**。よって「非 active だが `interaction_state=AwaitingHuman`」の run を拾う専用クエリを追加し、dashboard 側だけがそれを強調行として合流させる(pane が無くても表示する — 動作対象は pane ではなく PR)。

**通知手段はコメント投下ではない。** park は `interaction_state` + notify(会話タイムライン外)で表現し、PR 会話へは何も足さない。これは ADR 0008 の「検査履歴は会話タイムライン外(status + `<details>`)」原則と衝突しない。

## Rationale / 却下した代替案

- **(A) park 専用 `RunStatus` を新設** — scheduler・dashboard・daemon・cleaner など `RunStatus` を分岐する全経路に波及する。「run は Succeeded で終わった正常な workflow」という事実とも噛み合わない。過剰。却下。
- **(B) `meguri:needs-human` ラベルを貼る** — ADR 0008 の needs-human 昇格は auto-merge guard(Impl) **failure** に限定されている。findings は失敗ではなく**正常な人間ゲート**であり、ball 軸(ADR 0005)の needs-human とは意味が違う。加えて `needs-human` はループの締め出し(ADR 0007 のデッドロック注意)を招きうる。却下。
- **(C) PR に「レビュー待ちです」コメントを投下** — 会話タイムラインを汚し、ADR 0008 の会話タイムライン外原則に反する。却下。
- **(D) 永続マーカーで head 単位の厳密1回配送を保証** — notify 前に当該 run の `review.awaiting_human` イベント有無(または専用マーカー)を照会すれば、再実行・再起動をまたぐ重複を防げる。しかし emit と配送は原子的でない(先に emit すれば crash で通知欠落、先に配送すれば重複)ため厳密化しても保証は閉じず、best-effort チャネルの上に冪等機構を積むことになる。重複の害(同じ page がもう1回届く)< 欠落の害(park が無標識に戻る)。過剰。却下。
- **採用(interaction_state + notify)** — 既存レールの最小拡張で、run スコープの意味を workflow スコープへ広げるだけ。通知の重複抑止は既存 throttle にそのまま乗る(best-effort — Decision 参照)。

## Consequences

- `InteractionState::AwaitingHuman` の意味が「走行中 turn の待ち」から「run が終わった後の workflow 待ち」へ拡張される。両者は event 名(`turn.awaiting_human` / `review.awaiting_human`)で区別する。
- **parked な `interaction_state` のクリアが必要**になる。放置すると Succeeded run の `AwaitingHuman` が DB に残り、dashboard に古い park が滞留する。次 head の review 着手時(同一 issue の prior park を解消)および issue close 時(reaper)にクリアする。
- clean 手前の gate park は #132(`plan_delivery` / guard 有効化)着地後に有効化される。park シグナルのヘルパは kind 非依存に作り、#132 で `spec_reviewer → guard.rs` へ一般化される settle が findings/clean 両分岐から同一ヘルパを呼べるようにする。
- loops.md の ADR 索引(§5)更新は #132 着地時の doc 追随(§6)に合流させる — 本 ADR 単独では触らない。
