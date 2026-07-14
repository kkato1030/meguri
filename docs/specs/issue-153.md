# issue-153 spec — parked spec レビューを人間に能動シグナル化する

> ADR 参照について: 本文書で「ADR 0008」は `docs/adr/0008-symmetric-plan-impl-review-loop.md`(spec/impl ループ対称化、#132、**main 着地済み**)を指す。番号は `docs/adr/0008-agent-instructions-via-apm.md`(#137/APM)と重複しているため、必ずこの注記の意味で読むこと。

pr_reviewer(Plan)(ADR 0008 の対称化で旧 `spec_reviewer` を置換、#190 で guard → pr-reviewer に rename 済み)が findings を返すと、spec PR は `meguri:spec-reviewing` のまま静かに固まる。`settle` は `meguri/pr-review` の commit status(failure)を書き、レビューを PR 本文の `<details>` に折り込み(会話コメントは投下しない)、`meguri:working` を外し、run を `Succeeded` で終える(`src/engine/pr_reviewer.rs:665-709`)。同じ head は二度レビューされない(commit status が dedup key、`src/engine/pr_reviewer.rs:207-215`)ので、次の push があるまで PR は**無標識のまま据え置かれる** — `needs-human` も `awaiting_human` も notify も出ない。さらに `plan_delivery=separate`(既定、`src/config.rs:832-843`)では **clean でも park する**: `spec-reviewing → spec-ready` に遷移した後、spec PR をマージするのは人間の仕事であり(`spec_worker` は combined でしか discover せず — `src/engine/spec_worker.rs:66` — auto-merge も spec ラベルの付いた PR は arm しない(`src/engine/auto_merger.rs` の `BLOCKING_LABELS`)。handoff sweep(`src/engine/plan_handoff.rs`)が issue を `ready` に進めるのも**マージ後**)、これも無標識で待つ。

この spec の決定は一行で書ける。**pr_reviewer(Plan) が park する終端で、run を `Succeeded` のまま終えつつ `interaction_state=AwaitingHuman` をセットし、`deps.notifier` を直接叩き、`review.awaiting_human` を emit する。** 会話タイムラインには何も足さない。設計判断そのもの(なぜ専用 status でも label でもなく interaction_state + notify なのか)は **ADR 0009**(本 PR 同梱)に置いた。この spec は実装のための足場に徹する。

## 投資前に済ませた検証(issue が挙げた「唯一のリスク」)

issue は「`interaction_state=AwaitingHuman` が `run status=Succeeded` でも dashboard/notify に出るか。出なければ park を専用終端で表す小改修が要る」を実装前検証事項に挙げた。コードを読んで確定した:

- **notify — 出る(改修不要)。** 通知配送は run の status に一切依存しない imperative 呼び出しである(`src/notify/mod.rs` の `notify_awaiting_human` は throttle と gateway 配送だけを見る)。`deps.notifier` はエンジンから直接触れる(`src/engine/mod.rs:51`, `Deps::notifier`)。よって park 時に `Succeeded` でも通知できる。
- **dashboard — 出ない(小改修が要る)。** `meguri top` の `top_refresh`(`src/app.rs:921`)は `store.list_runs(true)` を読む。`active_only=true` の SQL は `status IN ('queued','running','interrupted')` に限られ(`src/store/runs.rs:574-582`)、`Succeeded` の run は行にすら現れない。したがって `▶` 強調(awaiting フラグ計算 `src/app.rs:963-972`、marker `:1027`)は出ない。`list_runs(true)` は scheduler も駆動対象抽出に使う共有関数(`src/engine/scheduler.rs:229,285` ほか)なので**意味を変えない**。→ 専用クエリで parked run を拾い、dashboard 側だけがそれを強調行に合流させる。

結論: 通知は既存レールにそのまま乗る。dashboard だけが「非 active だが待っている run」を拾えないので、そこを埋める。

## スコープ

- **In(今回この branch で実装):**
  - findings park の能動シグナル(`pr_reviewer::settle` の findings 分岐、`kind == Plan` のみ — 下記「変更するファイル」1.)。
  - clean park の能動シグナル(同 settle の clean 分岐、`kind == Plan` かつ `plan_delivery != Combined` のとき。combined は `spec_worker` が `spec-ready` を自動継続するので park ではない)。
  - park シグナルのヘルパ(interaction_state セット + notify + event emit)を **kind 非依存**に作る。
  - dashboard が parked run を強調行として拾えるようにする。
  - parked `interaction_state` のクリア(古い park を残さない)。
- **Out:** pr_reviewer(Impl) findings のシグナル(ADR 0008 §5 の auto-merge arm gate → needs-human 昇格という別経路が既にある。ヘルパは kind 非依存なので将来必要なら1行で呼べる)。`spec.auto_approve` ノブ(#132 の `plan_delivery` + `[review.guard]` 有効/無効に置換済み — config キーは #190 の rename 後も `guard` のまま)。findings park の**自動修正**(#188 の領分 — 下記)。

## #188(spec_fixer)との関係

findings park を「まず自動で直す」ループの欠落は #188 として別途起票されている(pr_reviewer(Plan) findings → planner の author lane を再駆動して修正 push → 既存 pr_reviewer が新 head を再レビュー、ラウンド上限超過で `needs-human`)。役割分担は: **#188 = 自動修正側、本 spec(#153)= 能動シグナル側**。

- 現時点では spec_fixer が存在しないため、findings park は即座に人間の仕事になる — 本 spec は findings 直後に page する。
- #188 着地後は、ラウンド上限内の findings は spec_fixer が拾うため findings 直後の page はノイズになる。その時点で findings 側の notify 呼び出しは「上限超過エスカレーション時」へ移す(#188 の受け入れ基準が本 spec の awaiting_human シグナルとの合流を明記している)。この調整は settle/spec_fixer の同じ縫い目を触る #188 実装側で行う — 本 spec のヘルパはそのまま再利用できる。
- 本 spec が守る不変条件は「**無標識の park を作らない**」: park した瞬間、それを拾う自動ループが無いなら人間に page が飛び、dashboard に出る。

## 変更するファイル

1. **`src/engine/pr_reviewer.rs`** — `settle()`(`:665-709`)で park ヘルパを呼ぶ。呼ぶのは `cp.kind == Kind::Plan` のときだけで、(a) findings 分岐(`spec-reviewing` を維持する現行動作のまま)、(b) clean 分岐は `deps.project.plan_delivery != PlanDelivery::Combined` の場合のみ(`spec-ready` 遷移後、人間の spec PR マージ待ちになるため)。順序: settle 内で park ヘルパ → settle 完了 → `flow::finish_pane`(`:350`)→ `WorkerOutcome::Succeeded` → `run_pr_reviewer` が `update_run_status(Succeeded)`(`:238-239`)。status 更新は interaction_state を触らないので残る。
2. **park ヘルパ(`src/engine/flow.rs` に置き、`pr_reviewer` から呼ぶ)** — 引数の run に対し:
   - `store.update_interaction_state(run_id, Some(AwaitingHuman))`,
   - `Notification` を組んで `deps.notifier.notify_awaiting_human(…)`(reason は下記の新規値。「見に行く先」は PR URL だが、`attach` は「pane に attach する shell command」という契約なので流用しない — 新設の `url` フィールドに載せる。契約変更の詳細は下記 5.),
   - `store.emit(Some(run_id), "review.awaiting_human", …)`(verdict / head / pr を data に載せる)。
   kind にも verdict にも依存させない(pr_reviewer の findings/clean 両分岐、将来の #188 上限超過エスカレーションから同一ヘルパを呼べる形)。
3. **`src/store/runs.rs`** — review park だけを拾う専用クエリ `list_parked_reviews()` を追加。条件は「**park ヘルパが実際に走った run**」であり、それを表すのは `review.awaiting_human` イベントの存在だけである:
   ```sql
   status = 'succeeded'
   AND interaction_state = 'awaiting_human'
   AND EXISTS (SELECT 1 FROM events e
               WHERE e.run_id = runs.id AND e.kind = 'review.awaiting_human')
   ```
   なぜ状態だけでは足りないか: `AwaitingHuman` は turn 側でも立つ(`turn.awaiting_human`)。pr_reviewer(Impl)、あるいは combined の pr_reviewer(Plan) が runtime/quiet で人間待ちになったあと、同じ run がレビューを書いて `Succeeded` で終わると、`update_run_status(Succeeded)` は `interaction_state` を消さないので状態は残る。これらは park ヘルパを**呼ばない**経路なので、dashboard に出してはいけない。両者を分ける唯一の印は「ヘルパが emit した `review.awaiting_human` イベントがあるか」。`loop_kind='pr-reviewer'` では Impl と combined-Plan の turn linger を除けないので使わない(将来 #188 の spec_fixer が別 loop から同ヘルパを呼んでも、イベント条件ならそのまま拾える)。`status='succeeded'` は異常終了への保険、`interaction_state='awaiting_human'` は「まだクリアされていない park」の担保。クリアは既存 `update_interaction_state(id, None)`(`:685-693`)を再利用。
4. **`src/app.rs`** — `top_refresh`(`:921`)に parked run を合流させる。parked 行は pane が無く(あるいは死んで)ても表示する(動作対象は PR)。既存の `awaiting_human` 強調(`▶`、フラグ計算 `:963-972`・marker `:1027`)と `render_top`(`:997`)はそのまま流用できる。active 行との run_id 重複は排除。
5. **`src/notify/mod.rs`** — 通知契約を「pane 前提」から「pane または web 上の待ち先」へ広げる。現行の `Notification.attach` は「pane に attach する shell command」(`src/notify/mod.rs:27-28`)、macOS 通知本文は `meguri attach <run_id>` 固定(`:144`)、webhook は `attach`/`attach_cli` を併記(`:175-176`)— parked run はこの契約に乗らない(pane は notify 直後の `finish_pane` で消え、動作対象は PR)ので、流用ではなく契約を変更する:
   - `Notification.url: Option<String>` を新設。parked review では PR URL を入れる(既存の turn 系エスカレーションは `None`)。
   - `Notification.attach` を `Option<String>` にする(`Some` = 生きた pane に attach する shell command、`None` = pane 無し)。parked run は `None` を渡す。既存の turn 系呼び出しは `Some(...)` に包むだけ。
   - `osascript_notification` の本文: `url` があれば `「{reason_label} — {url}」`、無ければ従来どおり `「{reason_label} — meguri attach {run_id}」`。
   - `webhook_payload`: `url` キーを追加(無ければ null)。`attach` は `None` なら null。`attach_cli` は pane がある場合(`attach` が `Some`)のみ載せる — 終了済み run への `meguri attach` は死に導線なので誘導しない。
   - `reason_label` に新 reason(例 `"spec_review_parked"`)の日本語ラベルを1アーム追加。`webhook_payload` の `event` は現状 `"turn.awaiting_human"` 固定 — reason で区別できるので必須ではないが、余裕があれば reason に応じて出し分ける(任意)。
6. **park クリア** — 次 head の review 着手時に古い park を解消する。`prepare_work` が新しい head を claim した箇所(`src/engine/pr_reviewer.rs:437-441` の `pr.claimed` emit 付近)で、同一 issue の prior な parked run の `interaction_state` を `None` に落とす。issue close 時は reaper の pane 回収経路に合わせてクリア(`reaper::sweep`、`src/engine/reaper.rs:522`)。

## 主要な決定

1. **run は `Succeeded` のまま。** park 専用 `RunStatus` は増やさない(ADR 0009 の代替案 A 却下)。
2. **通知は imperative(`deps.notifier`)で emit。** engine は turn の外なので `StoreControl::event` 経由ではない。event 名は `review.awaiting_human`(turn スコープの `turn.awaiting_human` と区別)。
3. **dashboard は専用クエリで parked を拾う。** `list_runs(true)` の意味は scheduler と共有のため変えない。拾う印は状態(`AwaitingHuman`)ではなく **`review.awaiting_human` イベントの存在** — これが「park ヘルパが走った」ことの唯一の証拠。状態だけで判定すると、pr-reviewer 自身の turn linger(Impl / combined-Plan)まで拾う。
4. **park クリアは「次 head 着手時」＋「issue close 時」。** さもないと dashboard に古い park が滞留する。
5. **clean park は `plan_delivery=separate` のときだけ配線する。** combined では `spec-ready` を `spec_worker` が自動継続するため park ではない(`src/engine/spec_worker.rs:66`)。ラベル遷移(`spec-reviewing → spec-ready`)自体は本 spec では触らない。
6. **findings 直後の page は #188 着地までの暫定で、着地後は上限超過時へ移る。** 移し替えは #188 側で行う(前節)。本 spec の不変条件は「無標識 park を作らない」ことであり、「findings で必ず page する」ことではない。
7. **同一 head への page は best-effort。** 既存の per-`run_id` throttle は**プロセス内メモリの時間窓**による重複抑止であり(`src/notify/mod.rs:65-79`、配送記録は永続化されない)、「head につき厳密に1回」の保証ではない。pr-reviewer run は head ごと新規なので通常運転では head=1回に一致するが、settle の中断・再開による同一 run の再実行、throttle 窓の経過、デーモン再起動では同じ head に再 page し得る。これは意図的に許容する — 重複 page は人間に同じ待ち状態を再提示するだけで無害、欠落 page こそ今回直す欠陥。永続マーカーによる厳密冪等は導入しない(ADR 0009 代替案 D 却下)。

## 受け入れ条件

1. pr_reviewer(Plan) の findings 判定で `settle` 後、その run は `status=Succeeded` かつ `interaction_state=AwaitingHuman`。clean 判定でも `plan_delivery=separate` なら同様。
2. park 時に `deps.notifier.notify_awaiting_human` が叩かれ、通知が PR を指す: macOS 本文と webhook `url` に PR URL が載り、pane 導線は出ない(webhook の `attach` は null、`attach_cli` キーは無し)。既存の turn 系通知(`url` 無し)の内容は従来どおり。
3. 重複抑止は best-effort: 同一プロセス・throttle 窓内の再 park では再配送されない(既存 per-`run_id` throttle)。settle 再実行・デーモン再起動をまたぐ head 単位の厳密1回は**保証しない**(主要な決定 7)。
4. `review.awaiting_human` イベントが emit される(verdict/head/pr を含む)。
5. `meguri top` に該当 run が `▶` 強調行で出る(`Succeeded` でも、pane が無くても)。逆に `AwaitingHuman` が残っているだけの run は出ない: turn 側で人間待ちになったまま stop/cancel/failed/skipped で終わった run も、pr-reviewer 自身の turn linger(Impl / combined-Plan が runtime/quiet で awaiting_human → 同 run が Succeeded)も、`review.awaiting_human` イベントを持たないので拾わない。
6. 次 head を push → 新 review 着手で、古い parked run の `interaction_state` がクリアされ、dashboard に残留しない。
7. **回帰なし:** clean 判定のラベル遷移(`spec-reviewing → spec-ready`)と findings 判定の `spec-reviewing` 維持は従来どおり。combined では clean で park シグナルが出ず、`spec_worker` の自動継続を妨げない。pr_reviewer(Impl) では park ヘルパは呼ばれない。
8. notify 無効時 / webhook 未設定時も落ちない(best-effort、既存挙動どおり)。

## テスト

- **unit(park ヘルパ):** ヘルパ呼び出しで対象 run の `interaction_state` が `AwaitingHuman` になり、notifier が叩かれること。`Deps::notifier` は `pub` フィールドなので、テストでは構築後に `FakeGateway`(`src/notify/fake.rs`)を包んだ `Notifier` へ差し替えて配送を観測する。
- **unit(notify 契約):** `url` 有りの `Notification` で osascript 本文が PR URL を含み `meguri attach` を含まないこと、webhook payload に `url` が載り `attach` が null・`attach_cli` キーが無いこと。`url` 無し(turn 系)の payload が従来形(`attach`/`attach_cli` 併記)を維持すること。
- **unit(store):** `list_parked_reviews()` が「Succeeded + AwaitingHuman + `review.awaiting_human` イベント有り」の run を返し、active run・interaction 無しの run を返さないこと。除外ケースを必ず入れる: (a) **cancelled/failed/skipped + AwaitingHuman**(turn 側の人間待ちを stop、または異常終了)、(b) **Succeeded + AwaitingHuman だが `review.awaiting_human` イベント無し**(= park ヘルパ未実行の turn linger。pr-reviewer の run でも起きる)。どちらも返さないこと。クリア(`update_interaction_state(id, None)`)で消えること。
- **integration(`pr_reviewer` settle findings):** `FakeForge` + `FakeMux` で plan PR の review=`findings` を通し、run が `Succeeded` + `interaction_state=AwaitingHuman`、notifier delivered==1、`review.awaiting_human` emit を確認(既存の `tests/pr_reviewer_test.rs` の道具立てをそのまま使える)。
- **integration(`pr_reviewer` settle clean):** `plan_delivery=separate` では clean でも park シグナル(interaction_state + notify)が出ること。`combined` では出ず、`spec-ready` 遷移だけが起きること。
- **clearing:** 同一 issue の2回目 head の `prepare_work`(claim)で prior parked run の `interaction_state` が None に落ちること。
- **回帰(impl):** `Kind::Impl` の findings では park ヘルパが呼ばれないこと。

## Done の目安

- 上記受け入れ条件を満たす実装とテストが緑。
- 設計判断は ADR 0009 に記録済み(本 PR 同梱)。spec(本ファイル)は実装着地時に刈られる(ADR 0001-specs-are-disposable-scaffolding)。
