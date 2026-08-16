# meguri — 現在のアーキテクチャと機能

> **この文書は「いまの meguri が実際に何であるか」を常に正確に映す地図である。**
> 機能を変える PR は必ずこれを同時に更新する(§20)。未来の計画は書かない
> —— それは [design/plan.md](plan.md) の仕事。ここに書いてよいのは、いまの main で
> 実際に動くものだけ。

最終更新: **v0.1 p1(データモデル + 永続化 + CLI)** 時点。

## いまできること

Intent → **Outcome Graph** の作成と表示を CLI で行える。Outcome の状態
(satisfied / ready / blocked)は保存せず**毎回導出**して表示する。

まだ無いもの: planning 対話(pane + proposal.json)/ Work の実行 / GitHub 連携 /
watch・reconciler。つまり「グラフを作って眺める」までで、実行系は未実装。

## コンポーネント(ソースと 1:1)

| モジュール | 責務 |
|---|---|
| `src/main.rs` | CLI(clap)。id の解釈(`o3`/`3`)と各コマンドのディスパッチ |
| `src/db.rs` | sqlite 接続とスキーマ(`~/.meguri/meguri.db`、`MEGURI_HOME` で移動可)。**保存は事実のみ** |
| `src/store.rs` | ドメイン型(Intent / Outcome / Verify / Work)と CRUD。requires 辺のサイクル防止もここ |
| `src/derive.rs` | satisfied / ready / blocked の**導出**(保存しない)。単体テストあり |
| `src/render.rs` | Outcome Graph の表示(テキスト / Mermaid) |

## ドメインモデル(§4/§5)

* **Intent** — 実現したいこと。グラフの根。
* **Outcome** — 到達したい状態(グラフのノード)。`statement` / `verify` / `requires`(前提辺)を持つ。
  * **verify** = 達成の確かめ方。3 種: `command`(コマンド exit 0)/ `human`(人が表明・sticky)/ `rollup`(まとめ節点=子が全て満たされたら)。
* **Work** — Outcome を満たす手段。`serves`(対象 Outcome)/ `objective` / `executor`(ai|human)/ `state` を持つ。p1 では登録のみ(実行は未実装)。

**保存する事実**: Intent / Outcome / requires 辺 / Work / human 充足表明。
**保存しない(導出)**: satisfied / ready / blocked。

### 導出のルール(`derive.rs`)

* satisfied: `human`=人の表明 / `command`=**p1 では常に未充足**(実行系=マージが無いため。p2/p3 で担当 Work のマージから満たされるようになる)/ `rollup`=子が全て satisfied。
* ready = 未充足 かつ requires が全て satisfied(→ ここに Work を起こせる)。
* blocked = 未充足 かつ 未充足の requires がある。

## CLI

```
meguri intent  add --title <t> [--description <d>]
meguri intent  ls
meguri outcome add --intent <i> --statement <s>
                   [--verify-command <cmd> | --milestone] [--requires o1,o2]
meguri outcome ls [--intent <i>]
meguri outcome satisfy   <o>      # human 充足表明を立てる(verify=human のみ)
meguri outcome unsatisfy <o>
meguri work    add --serves <o> --objective <s> [--executor ai|human]
meguri work    ls [--serves <o>]
meguri graph [--intent <i>] [--mermaid]
```

id は接頭辞つき(`i1`/`o3`/`w2`)でも数字だけでも受ける。

## 永続化 / ファイルシステム

```
~/.meguri/meguri.db      sqlite(MEGURI_HOME で移動可)
```

## 依存 crate

`clap`(CLI)/ `rusqlite`(bundled = システム sqlite 不要)/ `anyhow`。
最小に保つ(§20)。新しい crate はそれが解く問題が現れた増分で足す。

## 既知の割り切り(p1 の意図的な穴)

* `command` verify の Outcome は p1 では満たせない(実行系 = p2 待ち)。前提が揃えば ready にはなる。
* サイクル防止は `add_requires` にあるが、現行 CLI(`outcome add --requires` は既存ノードのみ参照)では実際にサイクルを作れないため、防御は休眠状態。
* Work は登録できるが実行しない。状態は `planned` のまま。
