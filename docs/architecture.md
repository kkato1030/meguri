# meguri — 現在のアーキテクチャと機能

> **この文書は「いまの meguri が実際に何であるか」を常に正確に映す地図である。**
> 機能を変える PR は必ずこれを同時に更新する(§20)。未来の計画は書かない
> —— それは [design/plan.md](plan.md) の仕事。ここに書いてよいのは、いまの main で
> 実際に動くものだけ。

最終更新: **v0.1 p2.2(Planning の pane 自動化)** 時点。

## いまできること

- Intent → **Outcome Graph** の作成と表示(状態は保存せず**毎回導出**)。
- **Planning 契約**(§7、ACP でなくファイル契約=§8、画面は読まない):
  - 手動: `plan prompt`(プロンプト出力)→ エージェントが `proposal.json` を書く →
    `plan diff`(検証)→ `plan apply`(承認・反映)。
  - **自動(pane): `plan run`** — mux(§8)で pane を開き、config の `agent` を起動し、
    猶予後にプロンプトを注入、`proposal.json` の出現を待って diff → 承認・反映まで一気通貫。
    pane は残す(§3.5)。

まだ無いもの: Work の実行 / GitHub 連携 / watch・reconciler。

## コンポーネント(ソースと 1:1)

| モジュール | 責務 |
|---|---|
| `src/main.rs` | CLI(clap)。id の解釈(`o3`/`3`)と各コマンドのディスパッチ |
| `src/config.rs` | `~/.meguri/config.toml`(`lang` / `agent`)。無ければ既定 |
| `src/db.rs` | sqlite 接続とスキーマ(`~/.meguri/meguri.db`、`MEGURI_HOME` で移動可)。**保存は事実のみ** |
| `src/store.rs` | ドメイン型(Intent / Outcome / Verify / Work)と CRUD。requires 辺のサイクル防止もここ |
| `src/derive.rs` | satisfied / ready / blocked の**導出**(保存しない)。単体テストあり |
| `src/render.rs` | Outcome Graph の表示(テキスト / Mermaid / HTML)。HTML は **dagre(層状レイアウトエンジン、`src/vendor/dagre.min.js` を埋め込み)**でレイアウト。クリックで関連チェーンにフォーカス再レイアウト・ホバー/選択強調・詳細パネル。自己完結(CDN 不要)でローカルで開く |
| `src/plan.rs` | Planning 契約: プロンプト生成 / `proposal.json` の検証(ref・needs)/ 承認反映 / **`run`(pane 起動→注入→harvest の一気通貫)**。単体テストあり |
| `src/mux.rs` | pane 供給(§8): pane を作る・1 行送る・生死を見る の trait + **tmux / herdr backend** + auto 選択(herdr が生きていれば herdr、いなければ tmux)。`plan run` から使う。両 backend の実機単体テストあり |

## ドメインモデル(§4/§5)

* **Intent** — 実現したいこと。グラフの根。
* **Outcome** — 到達したい状態(グラフのノード)。`statement`(短い到達状態)/ `description`(詳しい説明、任意、Intent と対称)/ `verify` / `requires`(前提辺)を持つ。
  * **verify** = 達成の確かめ方。3 種: `command`(コマンド exit 0)/ `human`(人が表明・sticky)/ `rollup`(まとめ節点=子が全て満たされたら)。
* **Work** — Outcome を満たす手段。`serves`(対象 Outcome)/ `objective` / `executor`(ai|human)/ `state` を持つ。p1 では登録のみ(実行は未実装)。

**保存する事実**: Intent / Outcome / requires 辺 / Work / human 充足表明。
**保存しない(導出)**: satisfied / ready / blocked。

### 導出のルール(`derive.rs`)

* satisfied: `human`=人の表明 / `command`=**p1 では常に未充足**(実行系=マージが無いため。p2/p3 で担当 Work のマージから満たされるようになる)/ `rollup`=子が全て satisfied。
* ready = 未充足 かつ requires が全て satisfied(→ ここに Work を起こせる)。
* blocked = 未充足 かつ 未充足の requires がある。

## CLI

主内容(タイトル / 宣言 / 目的)は**位置引数**。修飾は平易なフラグ。

```
meguri intent  add "<title>" [--description <d>]
meguri intent  ls
meguri outcome add "<statement>" [--intent <i>] [--description <d>] [--check "<cmd>" | --milestone] [--needs o1,o2]
meguri outcome ls   [--intent <i>]
meguri outcome show <o>              # statement / description / verify / needs をまとめて表示
meguri outcome done   <o>      # 達成を表明(verify=human のみ)
meguri outcome undone <o>
meguri work    add "<objective>" --for <o> [--by ai|human]
meguri work    ls   [--for <o>]
meguri graph [--intent <i>] [--mermaid]                  # text / mermaid は stdout
meguri graph [--intent <i>] --html [--out <path>] [--no-open]
                              # クリックで詳細の自己完結グラフを書いてブラウザで開く(既定 MEGURI_HOME/graph.html)

meguri plan prompt [--intent <i>] [--file <path>]        # planning プロンプトを出力
meguri plan diff   [--intent <i>] [--file <path>]        # proposal の追加内容を検証・表示
meguri plan apply  [--intent <i>] [--file <path>] [--yes]  # 承認して反映(additive)
meguri plan run    [--intent <i>] [--agent <cmd>] [--detach] [--grace-secs N] [--timeout-secs N] [--yes]
                              # pane 起動→agent 実行→プロンプト注入→(proposal 検知→diff→反映)
```

**proposal パスの解決**(prompt / diff / apply / run 共通):`--file` 最優先 → `--intent i<N>`
なら `MEGURI_HOME/proposals/i<N>.json`(session パス)→ どちらも無ければ `MEGURI_HOME/proposal.json`。
これで `plan run --intent i3` と `plan diff/apply --intent i3` のパスが一致する。

**モード**:
- **ワンショット(draft)**: `plan run --intent i3` — 起動して最初の proposal を待ち(blocking)、diff → 承認。
- **対話(反復)**: `plan run --intent i3 --detach` — 起動+注入して即返る。人間が pane で対話 →
  `plan diff --intent i3`(その時点の proposal を収穫)→ さらに対話 → `plan apply --intent i3`。
  launch と harvest を分離。pane は残す(§3.5)。

`proposal.json` のスキーマ:

```json
{ "intent": "i1",
  "outcomes": [
    { "ref": "<ローカル名>", "statement": "<到達状態>",
      "verify": {"kind":"human"} | {"kind":"command","command":"<cmd>"} | {"kind":"rollup"},
      "needs": ["<ref>" または "o<id>"] } ] }
```
`ref` は proposal 内のローカル名(needs から参照)。既存ノードは `o<id>` で参照。**additive**(追加のみ)。

- verify は **`--check "<cmd>"`=command / `--milestone`=rollup / 無指定=human(既定)**。
- `--intent` は省略可 — Intent が 1 件ならそれを使い、複数なら指定を求める(0 件はエラー)。
- id は接頭辞つき(`i1`/`o3`/`w2`)でも数字だけでも受ける。

## 永続化 / ファイルシステム

```
~/.meguri/meguri.db          sqlite(MEGURI_HOME で移動可)
~/.meguri/config.toml        設定(lang / agent、無ければ既定)
~/.meguri/proposal.json      手動 planning の作業ファイル(既定パス)
~/.meguri/proposals/i<N>.json  `plan run` の Intent 別 proposal(並行 Intent が衝突しない)
~/.meguri/plan-prompt-i<N>.md  `plan run` がエージェントに読ませるプロンプト
```

## 言語(2 軸)

- **chrome(meguri 自身の言葉: help / ラベル / エラー / プロンプト)= 英語固定**。
- **content(Outcome の statement 等、書く中身)= `config.toml` の `lang`**(自然言語名、
  既定 `"English"`)。planning プロンプトに「Write each statement in `<lang>`」と渡す。
  `lang = "日本語"` にすれば日本語運用に切替。翻訳はしない(同一グラフの両言語表示=
  bilingual は将来 (C) で。proposal の `statement` は前方互換に保つ)。

## 依存 crate

`clap`(CLI)/ `rusqlite`(bundled = システム sqlite 不要)/ `anyhow` /
`serde` + `serde_json`(proposal.json)/ `toml`(config.toml)。
最小に保つ(§20)。新しい crate はそれが解く問題が現れた増分で足す。

## 既知の割り切り(p1 の意図的な穴)

* `command` verify の Outcome は p1 では満たせない(実行系 = p2 待ち)。前提が揃えば ready にはなる。
* サイクル防止は `add_requires` にあるが、現行 CLI(`outcome add --requires` は既存ノードのみ参照)では実際にサイクルを作れないため、防御は休眠状態。
* Work は登録できるが実行しない。状態は `planned` のまま。
