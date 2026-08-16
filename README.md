# meguri

> **meguri is a delivery control plane that turns intent into an
> executable outcome graph and coordinates humans and AI agents toward the
> desired state.**

Intent を到達したい状態(Outcome)のグラフに変換し、人間と AI Agent による
実行・判断を通じて Desired State への到達を管理する Delivery Control Plane。
初期は local-first で始める(現時点の重心であって定義ではない — `docs/plan.md` §1)。

- **いま実際に動くもの**: [`docs/architecture.md`](docs/architecture.md)(現状の正確な地図)
- **これからの計画**: [`docs/plan.md`](docs/plan.md)

## 現状(v0.1: Planning)

Intent を立て、pane でエージェントと対話して Outcome Graph を提案させ、承認すると
グラフになる、まで動く。Outcome の状態(satisfied / ready / blocked)は保存せず
毎回**導出**する。Work の実行(v0.2)/ GitHub 連携(v0.3)はこれから。

```sh
cargo build
BIN=./target/debug/meguri

$BIN intent add "Make auth production-ready"
$BIN plan run            # pane でエージェント起動 → 対話で proposal.json → 承認で反映
$BIN graph --mermaid     # Outcome Graph を表示(状態は導出)
```

手動で回す場合は `plan prompt`(プロンプト出力)→ エージェントが `proposal.json` を
書く →`plan diff` →`plan apply`。詳細は [`docs/architecture.md`](docs/architecture.md)。

## 設定(`~/.meguri/config.toml`)

すべて任意で、ファイルやキーが無ければ既定値が使われる。

```toml
# Outcome の statement を書く言語(自然言語名)。既定 "English"。
# planning プロンプトにそのまま渡すだけなので、"日本語" / "Japanese" など自由。
# 翻訳はしない(切替は以後書かれる内容に効く)。
lang = "English"

# `plan run` が pane で起動するエージェント CLI(そのまま shell に打ち込む 1 行)。
# 既定はこれ。planning ではファイルを書くだけなので権限確認を省いている。
# 1 回だけ変えたいときは `meguri plan run --agent "<cmd>"` で上書きできる。
agent = "claude --dangerously-skip-permissions"
```

保存先は `MEGURI_HOME`(既定 `~/.meguri`)を変えれば移動できる。

---

このリポジトリは 2026-08-16 にリセットされ、ゼロから再出発した。
旧実装(v1: issue 駆動の autonomous loop / v2: 増分書き直し)の履歴と
ADR 群(失敗カタログ)は、リモートの archive ブランチに保存されている:
`archive/v1-main` / `archive/v2` / `archive/v2-preflight`。
