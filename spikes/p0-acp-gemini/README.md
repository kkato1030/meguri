# p0 スパイク — ACP 往復の検証(Gemini CLI)

計画 v0.1 の p0。**捨てコード**。目的は 1 つだけ:

> meguri が **ACP クライアント**として既存のコーディングエージェントを駆動し、
> 対話の最小往復(指示を送る → 返答が返る)が本当に成立するか。

これが通らなければ Planning Plane の設計を退路(headless 実行 + ファイル渡し)へ
切り替える必要があった。**通ったので、ACP 路線を継続してよい。**

## 動かし方

```sh
cargo run -- "Reply with one short sentence."
```

`gemini`(0.49.0 で確認)が PATH にあり、oauth 済みであること。
初回は Google ログインが要る場合がある。

## 何が確認できたか

- **ACP = JSON-RPC 2.0 を stdio で流すだけ**。SDK なしで手書きできた(このスパイクは
  serde_json のみ、非同期ランタイムなし)。
- **Gemini CLI はネイティブ ACP**(`gemini --acp`、adapter 不要)。
  Claude Code を相手にする場合は `@zed-industries/claude-code-acp`(npm)を
  子プロセスとして噛ませる(未検証・p2 でやる)。
- **往復の手順**:
  `initialize`(protocolVersion=1 / clientCapabilities / clientInfo)
  → `session/new`(cwd は絶対パス / mcpServers)→ sessionId を得る
  → `session/prompt`(sessionId / prompt=[{type:text,text}])
  → 返答は `session/update` 通知の連なりで **ストリーム**(`sessionUpdate` =
  `agent_message_chunk`、`content.text`)→ 最後に stopReason 付き応答(`end_turn`)。
- **authenticate は不要だった**(gemini の oauth がキャッシュ済み)。`initialize` の
  応答に authMethods が並ぶので、未認証なら `authenticate` を挟む分岐が要る。
- **エージェントは対話中にクライアントを呼び返す**ことがある(`fs/*`・
  `session/request_permission` 等)。本物のクライアントはこれらを実装するか安全に
  断る必要がある。スパイクは -32601 で断って詰まりを回避した。
- **`--skip-trust`** で fresh cwd の folder-trust ゲートを回避(v1/v2 の
  folder-trust / preflight の学びと同じ勘所)。

## p1 / p2 への含意

- ACP は使える。Planning Plane(§7)はこの上に載せられる。
- 本物の ACP クライアントが持つ表面積 = セッションのライフサイクル管理 / ストリーム
  更新の解釈 / **クライアント側メソッド(fs・permission)の実装**。ここが p2 の実装対象。
- content block と `session/update` の種別が、そのまま planning 対話の語彙になる。
- 並行エージェント(v1.0 の watch)では、1 エージェント = 1 stdout をブロッキング読み
  している今の形は足りず、async かスレッド/エージェントが要る。

## 開発環境メモ

- macOS に `timeout` は無い(`gtimeout` か `perl -e 'alarm N; exec @ARGV'`)。
