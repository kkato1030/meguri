# p0 スパイク — ACP 往復の検証(Claude / Gemini)

計画 v0.1 の p0。**捨てコード**。目的は 1 つだけ:

> meguri が **ACP クライアント**として既存のコーディングエージェントを駆動し、
> 対話の最小往復(指示を送る → 返答が返る)が本当に成立するか。

これが通らなければ Planning Plane の設計を退路(headless 実行 + ファイル渡し)へ
切り替える必要があった。**Claude・Gemini の両方で通ったので、ACP 路線を継続してよい。**

## 動かし方

```sh
cargo run -- "Reply with one short sentence."                      # 既定 = Claude
MEGURI_ACP_AGENT=gemini cargo run -- "..."                          # Gemini
MEGURI_ACP_AGENT=codex  MEGURI_CODEX_MODEL=<model> cargo run -- "..." # Codex
MEGURI_ACP_AGENT=cursor cargo run -- "..."                          # Cursor
MEGURI_ACP_DEBUG=1 ... cargo run -- "..."                           # 生の session/update を stderr に出す
```

## エージェント別の検証結果(2026-08-16)

| agent | ACP 経路 | handshake+session | prompt→返答 | 判定 |
|---|---|---|---|---|
| **Claude**(本命) | adapter `@zed-industries/claude-code-acp` | ✓ | ✓ テキスト返る | **使える** |
| Gemini | ネイティブ `gemini --acp` | ✓ | ✓ テキスト返る | 使える |
| Codex | adapter `@zed-industries/codex-acp` | ✓ | △ トランスポートは往復するが、生成が **adapter 埋め込み codex-core の版数**で失敗(下記) | ACP は OK・adapter 待ち |
| Cursor | 第三者製 `cursor-agent-acp`(0.1.1) | ✓ | ✗ end_turn は返るが **返答テキスト・session/update が一切来ない** | 現状使えない |

前提: 各エージェントの実体 CLI がログイン済みで PATH にあること。adapter は
`npm i -g @zed-industries/claude-code-acp @zed-industries/codex-acp cursor-agent-acp`。

### Codex の詳細
ACP は initialize → session/new → session/prompt → 構造化応答まで完全に往復する
(モデル API まで到達している)。ただしこのアカウントの Codex は既定モデルが
`gpt-5.6-luna` で、これが弾かれる("requires a newer version of Codex")。他モデル
(gpt-5-codex / o3 等)は "ChatGPT account では非対応"。

**根本原因(2026-08-16 追検証)**: `codex-acp` adapter は**自前の codex-core を埋め込んで
おり、システムの `codex` バイナリを使わない**。だからシステム `codex` を 0.147.0 に
上げても効かない。adapter は既に最新(0.16.0)で、その埋め込み codex-core が luna 未対応。
adapter に外部 codex を使わせるフラグも無い。→ **ACP トランスポートは問題ないが、
このアカウント(luna 専用)では codex-acp adapter が luna を載せるまで生成できない。
現状は Codex 待ち。** `codex update`(システム CLI)では解けない。

### Cursor の詳細
`cursor-agent-acp`(第三者製・v0.1.1)は handshake と prompt ライフサイクル
(end_turn)は通すが、**session/update 通知を 1 つも送らず、prompt 応答も
`{stopReason:end_turn}` のみでアシスタントのテキストを一切運ばない**。会話として
成立しないので現状は不採用。Zed 純正の cursor adapter は存在しない。

## 何が確認できたか

- **ACP = JSON-RPC 2.0 を stdio で流すだけ**。SDK なしで手書きできた(このスパイクは
  serde_json のみ、非同期ランタイムなし)。
- **相手ごとの起動**(結果は上の matrix):
  - **Claude / Codex / Cursor はネイティブ ACP を持たず adapter 経由**、**Gemini だけネイティブ**。
    adapter は実体 CLI を子プロセスとして起動する。
  - Zed 純正 adapter があるのは Claude(`@zed-industries/claude-code-acp`)と
    Codex(`@zed-industries/codex-acp`)。Cursor 用は第三者製 `cursor-agent-acp` のみ。
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
