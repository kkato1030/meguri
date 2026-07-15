# ADR 0020: 通知シンクはイベント駆動の best-effort — 完了契約から切り離す

- Status: accepted
- Date: 2026-07-14
- Issue: #205(通知シンク — エスカレーション/スケジュール異常/監視ラベルを webhook へ push)
- 関連: notify subsystem #7 / 集約エスカレーション ADR 0012 / schedules ADR 0009 / 最小主義 ADR 0001

## 文脈

meguri が人間の注意を要する状態(needs-human エスカレーション、スケジュール発火失敗、監視ラベル付き
issue 起票)になっても、人間へ push で届く経路が無かった。人間は GitHub をポーリングして気づくしかない。

一方、通知の下地は既にあった。issue #7 の `src/notify/`(NotifyGateway / throttle する Notifier /
macOS + `curl` webhook の SystemGateway)。ただし配線は payload 1 形状に固定され、しかも「人間を今
ページする」通知が 3 経路に散っていた — `turn.awaiting_human`、parked review の
`review.awaiting_human`、spec-fixer ラウンド上限の `spec_fixer.budget_exhausted`。3 つとも
`webhook_url` 設定時に無条件で飛び、絞る手段が無い。一方 needs-human ラベルのエスカレーション
(`escalation.raised`)もスケジュール異常(`schedule.failed` は未定義、`schedule.skipped` は emit
済だが未接続)も、このシンクには繋がっていない。「配信されすぎる既存通知」と「配信されない新規通知」が
同居していた。

## 決定

**通知は、既に store に emit されているイベントを config の allowlist で選び、best-effort で webhook へ
流すシンクである。** 具体的に、次を恒久的な設計判断として固定する。

1. **完了契約からの分離(不変条件)**。通知の送信可否は turn の成否・完了コントラクト・run の進行に
   **一切影響しない**。webhook 失敗は `tracing::warn!` のみ、リトライしない、run を止めない。meguri の
   「成功は独立検証される」不変条件に、外部通知チャネルを絡めない。

2. **source は emit・dispatch は notify に集約**。各イベント源(escalation.rs / scheduler_fire.rs 等)は
   従来どおり自分のイベントを emit し、その直後に notifier へ渡すだけ。allowlist 判定・payload 整形・
   throttle は notify モジュール 1 箇所に集約する。`store.emit` 低レイヤのフックや、escalation 集約点への
   全寄せは採らない(前者は責務が滲み、後者は schedule/label を通せず結局分散する)。

3. **既存 `[notifications]` を拡張し、新セクションをフォークしない**(ADR 0001 の最小主義)。`events`
   allowlist の既定は `["awaiting_human"]` で、既存 config を無改変のまま現行挙動に保つ(後方互換)。
   ここで `awaiting_human` トークンは**「人間を今ページする」既存 3 経路(`turn.awaiting_human` /
   `review.awaiting_human` / `spec_fixer.budget_exhausted`)を一つに束ねる**。store kind と 1:1 では
   なく、ユーザにとって意味が同じ通知を 1 トークンで扱う。これにより、今まで直接 notifier を叩いて
   allowlist の外にあった parked/budget 通知も同じ判定に乗り、既定では 1 つも落ちず、絞りたい人だけ
   `awaiting_human` を外せる。schedule は異常の粒度が違うので `schedule.failed`(発火失敗)と
   `schedule.skipped`(overlap スキップ)を別トークンにし、片方だけ購読できるようにする。

4. **webhook 種別は URL から自動判別、`kind` で明示上書き**。sink ごとに本文が違う(Slack /
   ntfy / 汎用 JSON)ため判別は必須だが、素の運用は URL だけで済ませ、非典型 endpoint だけ 1 行で救う。

5. **HTTP crate を足さず `curl` に shell-out**。GitHub(`gh`)と同じく、埋め込みクライアントではなく
   CLI へ委ねる既存規約を守る。

## 帰結

- 「meguri が詰まった/人間の番になった」が Slack/スマホに届き、GitHub ポーリングが要らなくなる。無人運用の
   実用性が一段上がる。
- 通知は非破壊だが冪等ではない(同じ通知が複数飛びうる)。throttle で連投を緩め、受け手はそれを前提にする。
- allowlist の既定と後方互換により、既存ホストは何もしなくても壊れない。新イベントは opt-in で足す。
- **切り分けた領域**: 人間起票 issue のラベル監視は、meguri が観測しない issue を拾う別機構(poll ループ)で
   あり、このイベント駆動シンクには含めない(別 issue)。本 ADR のラベル監視は meguri 自身が起票する issue に
   限る。
- 通知を判断に使わない設計(片方向・best-effort)ゆえ、将来チャネル(ntfy 以外・別 payload)を再コンパイルせず
   config だけで足せる余地が残る。
