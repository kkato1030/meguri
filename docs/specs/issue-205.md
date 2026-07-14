# issue-205 spec — 通知シンク: 選んだイベントを webhook へ push する

meguri が詰まっても、人間はいま GitHub をポーリングしないと気づけない。この spec の決定は
一行で書ける。**すでに store に emit されているイベントを、config の allowlist(`events`)で選び、
既存の `src/notify/` シンクから webhook へ流す。**

**用語を先に固定する。** 二つの層があり、名前を混ぜてはならない:

- **store event kind**(内部・emit 時の文字列): `turn.awaiting_human` / `escalation.raised` /
  新設の `schedule.failed`。
- **config `events` トークン**(公開・ユーザが toml に書く正規名): `awaiting_human` / `escalation` /
  `schedule.failed`。この spec・受け入れ基準・テスト戦略はすべてこの正規トークンで書く。

マッピングは実装が一箇所に持つ(notify モジュール):

| config `events` トークン(正規) | store event kind(内部) |
|---|---|
| `awaiting_human` | `turn.awaiting_human` |
| `escalation` | `escalation.raised` |
| `schedule.failed` | `schedule.failed` |

ラベル監視は `events` トークンではなく per-project `notify.labels` で別に指定する(下記 §5)。

## spec の深さ: design(理由）

uncertainty × blast radius で選ぶ。blast radius が広い — config の公開契約(`events`・
per-project `notify`)を増やし、notify / escalation / scheduler / doctor の 4 サブシステムに
またがり、**外部サービス(Slack 等)への新しい outbound 副作用**を足す。uncertainty も中〜高
(webhook 種別判別・発火点・ラベル監視の取得機構に実在の代替案がある)。よって normal ではなく
design 深度で書く。ただし永続 state / DB スキーマは一切触らない(config 追加は後方互換)ため、
migration & rollback は「無い」ことを明記する軽い節に留める。

## 現状(何が既にあるか)

このイシューは greenfield ではなく、**既存 notify subsystem の一般化**である。

- `src/notify/mod.rs`: `NotifyGateway` トレイト + throttle する `Notifier` + macOS(osascript)
  と webhook(`curl`)の `SystemGateway`。webhook 本文は `webhook_payload`(mod.rs:181-197)で
  **1 形状にハードコード**。テスト seam は `src/notify/fake.rs` の `FakeGateway`。
- config `[notifications]`(`src/config.rs:956-986`): `macos` / `webhook_url` / `throttle_secs`。
  **`events` リストは無く**、`turn.awaiting_human` の 1 イベントを無条件配信するだけ。
- 配信トリガは `StoreControl::event`(`src/engine/mod.rs:417-437`)の `kind == "turn.awaiting_human"`
  1 箇所のみ。
- HTTP クライアント crate は**無い**。webhook も GitHub(`gh`)も CLI に shell-out する規約
  (notify/mod.rs:90-91)。#205 も `curl` を踏襲し、新しい HTTP crate は足さない。

3 つのイベント源の成熟度は大きく違う(左列は store event kind):

| store event kind | 現状 | #205 でやること |
|---|---|---|
| `turn.awaiting_human`(pane/parked 待ち) | emit 済 + 唯一配信中 | allowlist 経由に一般化 |
| `escalation.raised`(needs-human ラベル) | `escalation.rs` が emit、**notifier 未接続** | シンクへ接続 |
| `schedule.failed`(発火失敗) | **イベントすら無い**(`tracing::warn!` のみ) | イベント新設 → シンクへ |
| ラベル監視 issue 起票 | 中央フックが無い | `create_issue` 境界フック(下記の割り切り） |

## アーキテクチャ影響 & 検討した代替案

### 1. 発火点: 各源が emit・dispatch は notify に集約

- **代替 A(棄却)**: `store.emit` を直接フックし、全イベントを横取りして allowlist で振り分ける。
  → DB 書き込みの低レイヤに配信ポリシを混ぜる。テストしづらく、責務が滲む。
- **代替 B(棄却)**: escalation.rs の集約点に全配信を寄せる。→ schedule.failed / label は
  そこを通らないので、結局分散する。
- **採用**: すでに各源が emit しているイベントは触らず、**emit と対で notifier を呼ぶ**
  （`StoreControl::event` の既存パターンと同型)。振り分け(allowlist 判定・payload 整形・
  throttle)は notify モジュールに 1 箇所集約する。source は「emit してから `deps.notifier` に
  渡す」だけ。escalation.rs / scheduler_fire.rs の各 emit 直後に 1 行足る形。

### 2. webhook 種別判別: URL 自動判別 + `kind` 明示上書き

sink ごとに本文が違う(Slack=`{"text": "..."}`、ntfy=プレーン本文 + ヘッダ、汎用=構造化 JSON)
ので、判別は必要。

- **採用**: URL ホストで自動判別(`hooks.slack.com` → slack、`ntfy.sh`/`/ntfy` → ntfy、
  それ以外 → 汎用 JSON)。self-host された Slack 互換 endpoint のために
  `kind = "slack" | "ntfy" | "json"` で明示上書きを許す(省略時 auto)。理由: 素の運用は URL
  だけで動き、非典型ケースだけ 1 行で救える。

### 3. config: 既存 `[notifications]` を拡張(新 `[notify]` を作らない)

イシューの sketch は `[notify]` / `webhook` だが、同義の `[notifications]` / `webhook_url` が
既にある。Rule of Three(ADR 0001 の最小主義)に従い**フォークしない**。既存セクションに
`events` と `kind` を足す。

- `events: Vec<String>`(正規トークン `awaiting_human` / `escalation` / `schedule.failed`、
  default `["awaiting_human"]`)。**後方互換の要**: 既存 config(webhook_url あり・events 無し)は
  default で現行と同一挙動になる。`escalation` / `schedule.failed` を足したい人だけ明示列挙する。
  未知トークンは config load 時に弾く(doctor でも報告)。
- per-project は `ProjectConfig` に `notify: Option<ProjectNotifyConfig> { labels: Vec<String> }`
  を追加(既存の per-project override 群と同じ `Option<T>` パターン、config.rs:1184-1213)。

### 4. throttle / Notification の一般化

現 `Notifier` は `run_id` で throttle し、`Notification` は awaiting_human 形(run_id/attach/url)。
schedule/label には run_id が無い。**dedup key を文字列に一般化**する:
`run_id`(awaiting_human)/ `schedule:<project>:<name>`(schedule.failed)/ `issue:<n>`
(escalation・label)。`Notification` は `event` / 人間向け 1 行 `text` / `dedup_key` /
optional `url` を持つ形へ広げ、awaiting_human 固有フィールドは text 生成側へ寄せる。
`notify_awaiting_human` は `notify(&Notification)` に改名(呼び出しは awaiting_human 1 箇所のみ）。

### 5. ラベル監視の割り切り(重要・レビューで詰めたい）

`human:todo` のようなラベルは **meguri が観測していない外部/人間起票の issue** を指しうる。meguri は
今 `meguri:ready`/`meguri:plan` しか list しないので、人間起票を拾うには**新しい poll ループ**が要る
（escalation/schedule のイベントシンクとは別機構)。

- **採用(v1)**: ラベル監視は `Forge::create_issue` 境界のフックに限る — **meguri 自身が起票する
  issue** に監視ラベルが載ったときだけ通知(schedule 起票・decompose の human 子など)。1 箇所で安く、
  イベントシンクと同じ best-effort 経路に乗る。
- **スコープ外 → 別 issue 化を推奨**: 人間起票 issue のラベル監視。理由は上記の通り別機構(poll
  ループ)で、本 issue の主眼(issue 起票を経由しないエスカレーション/スケジュール異常を拾う)とは
  独立に価値が出せる。※イシューの主動機は「issue 起票を経由しない」イベントの捕捉であり、ラベル監視は
  "追加監視" と明記されている。レビューで「人間起票の監視こそ本命」となれば、その poll ループは
  この spec から外し独立 issue に切る。

### 6. secret の置き場所

`webhook_url` は今 config.toml に平文。これは host 側 config(コミット対象でない)であり
repo 側 `meguri.toml` ではないので露出リスクは低い。ただし安全側の既定として
**`${ENV_VAR}` 展開を load 時にサポート**することを推奨(`webhook_url = "${MEGURI_WEBHOOK_URL}"`)。
小さく閉じた追加。レビューで「不要」となれば見送り可(key decision)。

## 変更箇所

1. **`src/config.rs`** — `NotificationsConfig`(956-986)に `events: Vec<String>`
   (default `["awaiting_human"]`)と `kind: Option<WebhookKind>` を追加。`WebhookKind` enum
   (`slack`/`ntfy`/`json`)を新設。`ProjectConfig`(1166-1229)に
   `notify: Option<ProjectNotifyConfig>`。`meguri init` テンプレ(config.rs:74-77 付近)を更新。
   採用すれば `webhook_url` の `${ENV}` 展開を load 時に。
2. **`src/notify/mod.rs`** — `Notification` を event/text/dedup_key/url 形に一般化。
   `webhook_payload` を kind 別 payload 整形(slack/ntfy/json)へ差し替え。`notify_awaiting_human`
   → `notify`、throttle を dedup_key ベースに。SystemGateway の `curl` 呼びは維持。
3. **`src/engine/escalation.rs`** — 3 helper の `escalation.raised` emit 直後に、allowlist が
   `escalation` を含むとき `deps.notifier.notify(...)`。`Deps` は既に `notifier` を持つ(mod.rs:41)。
4. **`src/engine/scheduler_fire.rs`** — `sweep`(54-65)の `Err` アームで `schedule.failed` を
   emit(現状 `tracing::warn!` のみ)+ notifier へ。
5. **`src/forge/*` or 呼び出し側** — `create_issue` で作られた issue のラベルが per-project
   `notify.labels` に該当すれば notifier へ(v1 のラベル監視)。フックは create_issue 呼び出しを
   束ねる薄いヘルパ、または各源。※新規ラベル定数は足さない(監視対象は任意の外部語彙)。
6. **`src/main.rs`** — `doctor_notify(cfg)` を新設(`doctor_schedules` に倣う)。webhook 未設定なら
   無言。設定済なら config 妥当性(URL/kind 解決)を検査し、`--probe` 時のみ**実テスト送信**
   （`--probe` は既に「実クォータ/ネットワークを使う」opt-in の前例、cli.rs:19-24)。
7. **`src/notify/fake.rs` / `tests/`** — `FakeGateway` で新イベントの配信記録を検証。
8. **`docs/adr/0018-notify-sink-event-driven-best-effort.md`** — 恒久判断(本 PR 同梱、下記)。

## 失敗時の扱い / observability

- **best-effort・run を止めない・リトライしない**。SystemGateway の既存規約どおり、`curl` 失敗は
  `tracing::warn!` のみ。通知失敗が完了コントラクトや turn の成否に影響してはならない(不変条件)。
- throttle は同一 dedup key の連投を抑える(既存 60s 既定を踏襲)。
- 各イベントは従来どおり store の events 表に残る。配信の成否は warn ログで観測(専用 metric は
  今回作らない — Rule of Three)。

## migration & rollback

- **永続 state / DB スキーマ変更なし**。既存 events 表を使うだけ。
- config は純追加・後方互換(`events` 既定 = 現行挙動)。ロールバックは `events` を空/未設定へ戻すか
  `webhook_url` を外すだけで即無効化。移行手順は不要。
- 外部副作用(webhook 送信)は非破壊・冪等でない点だけ注意(同じ通知が複数飛びうる = throttle で緩和)。

## test strategy

- notify 単体: kind 別 payload 整形(slack=`{"text"}` / ntfy=本文 / json=構造化)を assert。
  dedup_key throttle の境界(既存 `notify_test.rs` の paused-time パターンを踏襲)。
- escalation: `escalation.raised` 発火時、allowlist に応じて `FakeGateway` に配信/非配信されることを
  検証。allowlist 未指定なら escalation は飛ばない(後方互換)。
- schedule: `fire_one` を失敗させ `schedule.failed` の emit と配信を検証。
- label: `FakeForge` で監視ラベル付き create_issue → 配信、非該当ラベル → 非配信。
- doctor: `doctor_notify` が未設定で無言、設定済で妥当性 OK、`--probe` で送信を試みることを確認。
- 既存 `notify_test.rs` / `scheduler_test.rs` の非破壊。

## 受け入れ基準

1. `webhook_url` 設定・`events` 未設定の既存 config が、現行と同一(awaiting_human のみ配信)で動く。
2. `events = ["escalation"]` で、`meguri:needs-human` へのラベルエスカレーション(spec fixer 3 ラウンド赤
   等)が webhook に届く。allowlist から外せば届かない。
3. `events = ["schedule.failed"]` で、スケジュール発火失敗が `schedule.failed` として emit され配信される。
4. webhook 種別が URL から自動判別され、Slack には `{"text": ...}`、ntfy にはプレーン本文、汎用には
   構造化 JSON が飛ぶ。`kind` 明示で上書きできる。
5. per-project `notify = { labels = ["human:todo"] }` で、meguri が起票した該当ラベル issue が通知される。
6. 通知失敗(webhook 到達不能)が turn の成否・完了コントラクトに一切影響しない。
7. `meguri doctor` が notify 設定を検査し、`--probe` でテスト送信する。
8. `cargo fmt` / `clippy -D warnings` / `nextest` / `test --doc` が緑。既存 notify/scheduler テスト非破壊。

## スコープ外

- **人間起票 issue のラベル監視**(poll ループが要る別機構 → 別 issue 推奨、上記 5)。
- 双方向(Slack からの操作)。通知は一方向 push のみ。
- リッチ化(Block Kit 等)。text 1 本で足りる。
- 専用 metric / 配信履歴の永続化(warn ログで足りる)。
