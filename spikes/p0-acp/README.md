# p0 スパイク — ACP 往復の検証(Claude / Gemini)

計画 v0.1 の p0。**捨てコード**。目的は 1 つだけ:

> meguri が **ACP クライアント**として既存のコーディングエージェントを駆動し、
> 対話の最小往復(指示を送る → 返答が返る)が本当に成立するか。

これが通らなければ Planning Plane の設計を退路(headless 実行 + ファイル渡し)へ
切り替える必要があった。**Claude・Gemini の両方で通ったので、ACP 路線を継続してよい。**

## 動かし方

```sh
# 本命: Claude(claude-code-acp adapter 経由)
cargo run -- "Reply with one short sentence."

# Gemini(ネイティブ ACP)
MEGURI_ACP_AGENT=gemini cargo run -- "Reply with one short sentence."
```

前提:

- **Claude**: `claude-code-acp`(`npm i -g @zed-industries/claude-code-acp`)が PATH にあり、
  実体の `claude` バイナリ(例 `~/.local/bin/claude`)がログイン済みであること。
- **Gemini**: `gemini`(0.49.0 で確認)が PATH にあり oauth 済みであること。

## 何が確認できたか

- **ACP = JSON-RPC 2.0 を stdio で流すだけ**。SDK なしで手書きできた(このスパイクは
  serde_json のみ、非同期ランタイムなし)。
- **相手ごとの起動**:
  - **Claude はネイティブ ACP を持たず、adapter 経由**(`@zed-industries/claude-code-acp`
    0.16.2)。adapter が実体の `claude` を子として起動する。agentInfo は "Claude Code"、
    モデルは Opus/Sonnet/Haiku が見えた。
  - **Gemini はネイティブ ACP**(`gemini --acp`、adapter 不要)。
  - **Codex** はネイティブ ACP 無し(`codex` の subcommand に acp 無し。mcp-server は別物)。
    adapter は `@zed-industries/codex-acp`(npm、0.16 系)が存在する。**未検証**。
- **往復の手順**:
  `initialize`(protocolVersion=1 / clientCapabilities / clientInfo)
  → `session/new`(cwd は絶対パス / mcpServers)→ sessionId を得る
  → `session/prompt`(sessionId / prompt=[{type:text,text}])
  → 返答は `session/update` 通知の連なりで **ストリーム**(`sessionUpdate` =
  `agent_message_chunk`、`content.text`)→ 最後に stopReason 付き応答(`end_turn`)。
- **Claude 特有の落とし穴**: **Claude セッションの中から Claude を起動すると入れ子ガードで
  弾かれる**("cannot be launched inside another Claude Code session")。子プロセスで
  `CLAUDECODE`(と `CLAUDE_CODE_ENTRYPOINT`)を unset して回避する。meguri を Claude の外で
  動かす通常運用では起きないが、Claude の中で dogfood する時に踏む。
- **authenticate は不要だった**(Claude は claude ログイン済み / Gemini は oauth キャッシュ)。
  `initialize` の応答に authMethods が並ぶので、未認証なら `authenticate` を挟む分岐が要る
  (Claude は `claude /login`、Gemini は Google ログイン等)。
- **エージェントは対話中にクライアントを呼び返す**ことがある(`fs/*`・
  `session/request_permission` 等)。本物のクライアントはこれらを実装するか安全に
  断る必要がある。スパイクは -32601 で断って詰まりを回避した(テキスト返答だけなら通る)。

## p1 / p2 への含意

- ACP は使える。Planning Plane(§7)はこの上に載せられる。相手は **Claude を第一、Codex を退避**。
- 本物の ACP クライアントが持つ表面積 = セッションのライフサイクル管理 / ストリーム
  更新の解釈 / **クライアント側メソッド(fs・permission)の実装**。ここが p2 の実装対象。
  Claude は実際にツールを使うので、fs/permission の実装は Gemini より早く必要になる。
- エージェント起動レシピ(program / args / env 除去)は相手ごとに違う。実装では
  「エージェント抽象」を後で用意するときの最小差分がこれ(program・args・env_remove)。
- 並行エージェント(v1.0 の watch)では、1 エージェント = 1 stdout をブロッキング読み
  している今の形は足りず、async かスレッド/エージェントが要る。

## 開発環境メモ

- macOS に `timeout` は無い(`gtimeout` か `perl -e 'alarm N; exec @ARGV'`)。
- `claude-code-acp` の初回取得は `npx` だと重い。`npm i -g` で先に入れておくと速い。
