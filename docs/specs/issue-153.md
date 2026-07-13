# issue-153 spec — parked spec レビューを人間に能動シグナル化する

`spec_reviewer` が findings を返すと、PR は `meguri:spec-reviewing` のまま静かに固まる。`settle` は review コメントを1回投下し、`meguri:working` を外し、run を `Succeeded` で終える(`src/engine/spec_reviewer.rs:627-659`)。同じ head は二度レビューされない(head-sha マーカーの冪等性)ので、人間が fix を push するまで PR は**無標識のまま永久に据え置かれる** — `needs-human` も `awaiting_human` も notify も出ない。

この spec の決定は一行で書ける。**reviewer/guard が park する終端で、run を `Succeeded` のまま終えつつ `interaction_state=AwaitingHuman` をセットし、`deps.notifier` を直接叩き、`review.awaiting_human` を emit する。** 会話タイムラインには何も足さない。設計判断そのもの(なぜ専用 status でも label でもなく interaction_state + notify なのか)は **ADR 0009**(本 PR 同梱)に置いた。この spec は実装のための足場に徹する。

## 投資前に済ませた検証(issue が挙げた「唯一のリスク」)

issue は「`interaction_state=AwaitingHuman` が `run status=Succeeded` でも dashboard/notify に出るか。出なければ park を専用終端で表す小改修が要る」を実装前検証事項に挙げた。コードを読んで確定した:

- **notify — 出る(改修不要)。** 通知配送は run の status に一切依存しない imperative 呼び出しである(`src/notify/mod.rs` の `notify_awaiting_human` は throttle と gateway 配送だけを見る)。`deps.notifier` はエンジンから直接触れる(`src/engine/mod.rs:46`, `Deps::notifier`)。よって park 時に `Succeeded` でも通知できる。
- **dashboard — 出ない(小改修が要る)。** `meguri top` の `top_refresh` は `store.list_runs(true)` を読む。`active_only=true` の SQL は `status IN ('queued','running','interrupted')` に限られ(`src/store/runs.rs:414-428`)、`Succeeded` の run は行にすら現れない。したがって `▶` 強調(`src/app.rs:623-624,672`)は出ない。`list_runs(true)` は scheduler も駆動対象抽出に使う共有関数(`src/engine/scheduler.rs:50,225` ほか)なので**意味を変えない**。→ 専用クエリで parked run を拾い、dashboard 側だけがそれを強調行に合流させる。

結論: 通知は既存レールにそのまま乗る。dashboard だけが「非 active だが待っている run」を拾えないので、そこを埋める。

## スコープ

- **In(今回この branch で実装):**
  - findings park の能動シグナル(`spec_reviewer::settle` の findings 分岐)。
  - park シグナルのヘルパ(interaction_state セット + notify + event emit)を **kind 非依存**に作る。
  - dashboard が parked run を強調行として拾えるようにする。
  - parked `interaction_state` のクリア(古い park を残さない)。
- **Forward(#132 依存、今回は配線しない):** clean 手前の gate park。#132 / ADR 0008 の `plan_delivery=separate`(人間が spec PR をマージ)が着地して初めて clean が「人間待ち」になる。現行モデルでは clean → `spec-ready` → `spec_worker` が自動継続するため park ではない。ヘルパを再利用可能に作っておき、#132 で `guard.rs` の settle が findings/clean 両分岐から同一ヘルパを呼べる形にする。
- **Out:** `spec.auto_approve` ノブ(#132 の `plan_delivery` + guard 有効/無効に置換済み)。combined モードで人間ゲートを挟むかの方針決定(必要なら #132 側)。

## #132 との統合方針

#132(spec/impl ループ対称化、PR #140)は本 branch のベース(`main`)にまだマージされていない。よって実装は**現行の `spec_reviewer.rs` に載せる**。park ヘルパを kind 非依存に作れば、#132 で `spec_reviewer → guard.rs` へ settle が一般化される際に findings/clean 両分岐から呼べる。#132 と統合するか follow-up にするかは #132 の着地状況を見て着手時に判断する(issue 記載どおり)。

## 変更するファイル

1. **`src/engine/spec_reviewer.rs`** — `settle()` の findings 分岐(`review == Findings`、PR コメント投下後 `spec-reviewing` を維持する箇所、`:652-653` 付近)で park ヘルパを呼ぶ。`clean` 分岐は現行モデルでは park しないため呼ばない(#132 後に `guard.rs` 側で clean 分岐へ追加)。
2. **park ヘルパ(`src/engine/flow.rs` に置き、`spec_reviewer` から呼ぶ)** — 引数の run に対し:
   - `store.update_interaction_state(run_id, Some(AwaitingHuman))`,
   - `Notification` を組んで `deps.notifier.notify_awaiting_human(…)`(reason は下記の新規値、`attach` は PR URL を入れて「見に行く先」を PR に向ける),
   - `store.emit(Some(run_id), "review.awaiting_human", …)`(verdict / head / pr を data に載せる)。
   `settle` が `finish_pane` → `Succeeded` を返す**前**に呼ぶ(順序: park ヘルパ → `finish_pane` → `WorkerOutcome::Succeeded` → `run_spec_reviewer` が `update_run_status(Succeeded)`。status 更新は interaction_state を触らないので残る)。
3. **`src/store/runs.rs`** — 非 active だが待っている run を拾う専用クエリ `list_parked_awaiting_human()`(`interaction_state='awaiting_human' AND status NOT IN ('queued','running','interrupted')`)を追加。クリアは既存 `update_interaction_state(id, None)` を再利用。
4. **`src/app.rs`** — `top_refresh` に parked run を合流させる。parked 行は pane が無く(あるいは死んで)ても表示する(動作対象は PR)。既存の `awaiting_human` 強調(`▶`)と `render_top` はそのまま流用できる。active 行との run_id 重複は排除。
5. **`src/notify/mod.rs`** — `reason_label` に新 reason(例 `"spec_review_parked"`)の日本語ラベルを1アーム追加。`webhook_payload` の `event` は現状 `"turn.awaiting_human"` 固定 — reason で区別できるので必須ではないが、余裕があれば reason に応じて出し分ける(任意)。
6. **park クリア** — 次 head の review 着手時に古い park を解消する。`prepare_work` が新しい head を claim した箇所(`src/engine/spec_reviewer.rs:401-408` の `pr.claimed` emit 付近)で、同一 issue の prior な parked run の `interaction_state` を `None` に落とす。issue close 時は reaper の pane 回収経路に合わせてクリア(`reaper::sweep`)。

## 主要な決定

1. **run は `Succeeded` のまま。** park 専用 `RunStatus` は増やさない(ADR 0009 の代替案 A 却下)。
2. **通知は imperative(`deps.notifier`)で emit。** engine は turn の外なので `StoreControl::event` 経由ではない。event 名は `review.awaiting_human`(turn スコープの `turn.awaiting_human` と区別)。
3. **dashboard は専用クエリで parked を拾う。** `list_runs(true)` の意味は scheduler と共有のため変えない。
4. **park クリアは「次 head 着手時」＋「issue close 時」。** さもないと dashboard に古い park が滞留する。
5. **clean 手前 gate は #132 着地後。** ヘルパを kind 非依存に作り、`guard.rs` の clean/findings 両分岐から呼べるようにするところまでを今回やる。
6. **head=run 単位の1回 page** は既存の per-`run_id` throttle にそのまま乗る(reviewer/guard run は head ごと新規)。追加の冪等機構は要らない。

## 受け入れ条件

1. findings 判定で `settle` 後、その run は `status=Succeeded` かつ `interaction_state=AwaitingHuman`。
2. park 時に `deps.notifier.notify_awaiting_human` が head につき1回叩かれる(新 head = 新 run_id なので既存 throttle と整合)。
3. `review.awaiting_human` イベントが emit される(verdict/head/pr を含む)。
4. `meguri top` に該当 run が `▶` 強調行で出る(`Succeeded` でも、pane が無くても)。
5. 次 head を push → 新 review 着手で、古い parked run の `interaction_state` がクリアされ、dashboard に残留しない。
6. **回帰なし:** clean 判定は現行モデルでは park しない(`spec-ready` へ遷移し `spec_worker` が自動継続する)ことを維持する。
7. notify 無効時 / webhook 未設定時も落ちない(best-effort、既存挙動どおり)。

## テスト

- **unit(park ヘルパ):** ヘルパ呼び出しで対象 run の `interaction_state` が `AwaitingHuman` になり、notifier が叩かれること。`Deps::notifier` は `pub` フィールドなので、テストでは構築後に `FakeGateway`(`src/notify/fake.rs`)を包んだ `Notifier` へ差し替えて配送を観測する。
- **unit(store):** `list_parked_awaiting_human()` が Succeeded+AwaitingHuman の run を返し、active run や interaction 無しの run を返さないこと。クリア(`update_interaction_state(id, None)`)で消えること。
- **integration(`spec_reviewer` settle findings):** `FakeForge` + `FakeMux` で review=`findings` を通し、run が `Succeeded` + `interaction_state=AwaitingHuman`、notifier delivered==1、`review.awaiting_human` emit を確認。
- **clearing:** 同一 issue の2回目 head の `prepare_work`(claim)で prior parked run の `interaction_state` が None に落ちること。
- **回帰(clean):** review=`clean` では park ヘルパが呼ばれず、`spec-ready` 遷移と `spec-reviewing` 除去が従来どおり起きること。

## Done の目安

- 上記受け入れ条件を満たす実装とテストが緑。
- 設計判断は ADR 0009 に記録済み(本 PR 同梱)。spec(本ファイル)は実装着地時に刈られる(ADR 0001-specs-are-disposable-scaffolding)。
