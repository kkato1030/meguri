# meguri — 現在のアーキテクチャと機能

> **この文書は「いまの meguri が実際に何であるか」を常に正確に映す地図である。**
> 機能を変える PR は必ずこれを同時に更新する(§20)。未来の計画は書かない
> —— それは [design/plan.md](plan.md) の仕事。ここに書いてよいのは、いまの main で
> 実際に動くものだけ。分岐点での設計判断は [docs/adr/](adr/) に凍結する。

最終更新: **v0.2 o22 検証落ちの上限付き fix turn 差し戻し(meguri 自作 feature + レビュー修正)** 時点。

## いまできること

- Intent → **Outcome Graph** の作成と表示(状態は保存せず**毎回導出**)。
- **Planning 契約**(§7、ACP でなくファイル契約=§8、画面は読まない):
  - 手動: `plan prompt`(プロンプト出力)→ エージェントが `proposal.json` を書く →
    `plan diff`(検証)→ `plan apply`(承認・反映)。
  - **自動(pane): `plan run`** — mux(§8)で pane を開き、config の `agent` を起動し、
    猶予後にプロンプトを注入、`proposal.json` の出現を待って diff → 承認・反映まで一気通貫。
    pane は残す(§3.5)。

- **Work の実行(本線が一周する)**: `meguri run <o>` が ready Outcome から
  Work を起こし(o14)、bare から隔離 worktree を切り(o13)、その pane でエージェントを
  起動して実装プロンプトを注入(o15)、`.meguri/result.json` の出現をポーリングして
  完了検知(o16、画面は読まない)、success 報告時に **meguri 側の独立検証①②③
  (clean tree=o17 / commit 進行=o18 / check_command=o19)** を rollup し、
  **全部 pass なら `verified`(検証済み commit を Artifact=branch @ sha として記録=o21)/
  一つでも落ちれば `rework` に gate(o20)**。
- **launch / harvest の分離**(o27): harvest 芯(検証→gate→Artifact)は `finalize_work`(**pane 不要**、
  result.json と git 状態だけ)。**`meguri run` は既定で detach** — pane を開いてエージェント CLI を
  起動したら **即返る(〜1s)**。grace 待ちも実装プロンプトの注入もしない(state=`running`)。
  **注入と harvest は `meguri watch`(最小 reconciler)が担う**: running Work を走査し、result が
  出ていれば harvest、まだなら保存した pane ハンドルで **初回発見時に注入 → 以降 nudge**(上限付き)。
  `--wait` はその場で grace→注入→harvest まで同期でやる(従来挙動)。watch の既定は running が
  捌けるまでループ、`--once` で 1 パス(注入と harvest は別パスになりうる)。
- **ローカル accept**(Human Gate): `meguri accept <w>` で verified Work を受理 →
  serve 先 Outcome が **satisfied**(導出)→ 後続 Outcome が **ready** になる。
  これで `run → verified → accept → 次が ready` が一周する。

まだ無いもの: fix turn 差し戻し(o22)/ 沈黙・timeout・pane 死亡の失敗経路(o23-o25)/
accept 時の worktree・pane の後片付け / GitHub 連携 / watch・reconciler。

## コンポーネント(ソースと 1:1)

| モジュール | 責務 |
|---|---|
| `src/main.rs` | CLI(clap)。id の解釈(`o3`/`3`)と各コマンドのディスパッチ。run の実行は **launch(pane 起動+注入)** と **harvest 芯(`finalize_work`: result → 検証 → gate → Artifact、**pane 不要**)** に分離済み(将来の reconciler が harvest を再利用するため) |
| `src/config.rs` | `~/.meguri/config.toml`(`lang` / `agent`)。無ければ既定 |
| `src/db.rs` | sqlite 接続とスキーマ(`~/.meguri/meguri.db`、`MEGURI_HOME` で移動可)。**保存は事実のみ** |
| `src/store.rs` | ドメイン型(Intent / Outcome / Verify / Work)と CRUD。requires 辺のサイクル防止もここ |
| `src/derive.rs` | satisfied / ready / blocked の**導出**(保存しない)。command/human は **accept 済み Work を持てば satisfied**(`accepted` id 集合を受け取る)。単体テストあり |
| `src/render.rs` | Outcome Graph の表示(テキスト / Mermaid / HTML)。HTML は **dagre(層状レイアウトエンジン、`src/vendor/dagre.min.js` を埋め込み)**でレイアウト。クリックで関連チェーンにフォーカス再レイアウト・ホバー/選択強調・詳細パネル。自己完結(CDN 不要)でローカルで開く |
| `src/plan.rs` | Planning 契約: プロンプト生成 / `proposal.json` の検証(ref・needs)/ 承認反映 / **`run`(pane 起動→注入→harvest の一気通貫)**。単体テストあり |
| `src/gitops.rs` | **v0.2 execution の git 土台**: 管理 repo の **bare clone**(`bare_clone` / `fetch`、`--mirror` は使わず remote-tracking を張る)と、bare/通常 repo から **隔離 worktree**(o13、base SHA 記録・`.meguri/` を共有 exclude へ)。**worktree の base は fetch で更新される `origin/<branch>`**(bare の local `refs/heads/<branch>` は clone 時から動かないため、そこから切ると古い base になる)。実 git の単体テストあり |
| `src/mux.rs` | pane 供給(§8): pane を作る(`cwd` 指定可=execution は worktree で開く)・1 行送る・生死を見る・**attach 案内**(`attach_hint`: tmux は `tmux attach -t <s>`、herdr は `herdr`)の trait + **tmux / herdr backend** + auto 選択(herdr が生きていれば herdr、いなければ tmux)。`plan run` / `meguri run` から使う。両 backend の実機単体テストあり |
| `src/work.rs` | o22: 検証落ちの**上限付き fix turn** 方針。`FixTurn`(Retry/GiveUp)と `decide(spent, checks)`(`FIX_TURN_MAX=3` まで Retry、超えたら GiveUp)。落ちた検証子だけを診断に載せた差し戻し文を作る。pane/git 非依存の**純粋方針**(注入・state 遷移は harvest 側)。単体テストあり |
| `src/verify.rs` | meguri 側の**独立検証**(§9.3、trust-but-verify): 各検証子は `Check{name,pass,detail}` を返す。o17 = `clean_tree`(worktree に未コミット/追跡外が残っていないか。`.meguri/` は exclude 済みで無視)、o18 = `commits_ahead`(spawn 時に記録した base SHA より commit が進んでいるか=何も作らず report した空 worktree を弾く)、o19 = `check_command`(Outcome の verify=command を worktree で実行し exit 0 を要求。human/rollup は None=対象外。落ちたら stderr 末尾を添える)。`run_all` が適用可能な検証子を集め、`all_pass` で rollup(o20)。実 git の単体テストあり |
| `src/exec.rs` | v0.2 execution の**実装プロンプト**(完了契約、§9): spawn 済み Work のエージェントに「この worktree で実装 → commit → `.meguri/result.json` を書く」を指示(verify 種別ごとに DoD を出し分け)。加えて **result.json の読み取り**(`WorkResult{status,summary}`、部分書き込みは未完了扱い)と status→Work state の対応(o16)。画面は読まず result.json で完了を判定する契約。単体テストあり |

## ドメインモデル(§4/§5)

* **Intent** — 実現したいこと。グラフの根。
* **Outcome** — 到達したい状態(グラフのノード)。`statement`(短い到達状態)/ `description`(詳しい説明、任意、Intent と対称)/ `verify` / `requires`(前提辺)を持つ。
  * **verify** = 達成の確かめ方。3 種: `command`(コマンド exit 0)/ `human`(人が表明・sticky)/ `rollup`(まとめ節点=子が全て満たされたら)。
* **Work** — Outcome を満たす手段。`serves`(対象 Outcome)/ `objective` / `executor`(ai|human)/ `state` / spawn 時の worktree 情報(`worktree_path` / `branch` / `base_sha`)を持つ。`meguri run` で ready Outcome から起こし(o14)、その worktree の pane にエージェントを起動して実装プロンプトを注入(o15、state=`running`)、`.meguri/result.json` の出現を待って報告 status を state に反映する(o16)。注入は CLI(例: Claude Code)の cold-start 前だと落ちるので、**result が出るまで `--nudge-secs` 間隔で最大 3 回まで再注入**する。pane は detached な場所で開くため、`run` は覗くための **attach 案内**を表示する。
  * **state の流れ**: `planned`(add_work 直後)→ `running`(o15 起動)→ report 検知(o16)。report が **success なら meguri 側の独立検証(`verify.rs`、o17-o19)を rollup(o20)して gate**: 全 pass=`verified`(そのとき `artifact_sha` に検証済み commit を記録=o21)/ 一つでも落ち=**上限付き fix turn(o22)**: 落ちた検証の診断を同じエージェントに差し戻し(`works.fix_turns` を進める)、`FIX_TURN_MAX` まで回して尽きたら `rework`(人間へ)。差し戻し中は state=`running` のまま watch が harvest し続ける。report が failure=`failed` / needs_human=`needs_human`。pane 死亡・timeout では `running` のまま残す(pane も残す、§3.5。詳細な失敗経路は o23-o25)。**`verified` を `meguri accept` で受理すると、Outcome に受理事実(`acceptances`)が貼られ**、serve 先 Outcome が satisfied になる(Work state も `accepted` にするが、それは運用記録で根拠ではない)。**Work を掃除しても satisfied は退行しない**(ADR 0002)。
  * **Artifact**(o21): verified な Work の `artifact_sha`(= worktree HEAD)。ブランチ `meguri/w<id>` は bare clone に残るので `branch @ sha` が耐久成果物になる。`work ls` に表示。GitHub PR 投影(v0.3)の material。
* **Intent は repo に紐付く**(`repo_id`、任意)。別 Intent → 別 repo = マルチレポ。

**保存する事実**: Intent / Outcome / requires 辺 / Work / human 充足表明 / **受理(`acceptances`)**。
**保存しない(導出)**: satisfied / ready / blocked。

**受理(`acceptances`、ADR 0002)**: satisfied の根拠となる **Outcome に貼る耐久事実**(`outcome_id`, 由来 `work_id?`, `repo_id?`, `artifact_sha?`)。由来は 2 通り: **verified Work の accept**(`work_id` あり)か、**人手の満たし表明 `outcome done`**(`work_id` NULL、command 含む rollup 以外の任意 Outcome を run 抜きで消し込む=o28)。**Work を掃除しても退行しない**(`work_id` は情報用・FK なし)。Outcome ごと 0..N 行(複数 artifact / 複数リポで満たす将来に開く)。`works.state='accepted'` は運用状態として残すが根拠ではない。旧データは起動時に backfill。

### 導出のルール(`derive.rs`)

* satisfied: `human`=人の表明**または**受理あり / `command`=**受理があれば満たされる**(verified→`meguri accept`=ローカル Human Gate。GitHub 化は v0.3)/ `rollup`=子が全て satisfied。
* ready = 未充足 かつ requires が全て satisfied(→ ここに Work を起こせる)。
* blocked = 未充足 かつ 未充足の requires がある。
* 導出は `accepted`(受理を持つ Outcome の id 集合 = `acceptances` から)を受け取る。当面の満たし条件は「受理 1 つ以上」(複数 artifact の AND 満たしは未・ADR 0002 の open)。

## CLI

主内容(タイトル / 宣言 / 目的)は**位置引数**。修飾は平易なフラグ。

```
meguri repo    add <name> --from <url|path> [--branch <b>]  # bare clone を作って登録
meguri repo    ls | fetch <name> | rm <name>
meguri intent  add "<title>" [--description <d>] [--repo <name>]
meguri intent  ls
meguri intent  edit <i> [--title <t>] [--description <d>] [--repo <name>]
meguri intent  rm   <i>              # 配下の Outcome / 辺 / Work(+ その worktree 実体)ごと削除
meguri outcome add "<statement>" [--intent <i>] [--description <d>] [--check "<cmd>" | --milestone] [--needs o1,o2]
meguri outcome ls   [--intent <i>]
meguri outcome show <o>              # statement / description / verify / needs をまとめて表示
meguri outcome edit <o> [--statement <s>] [--description <d>] [--check <cmd>|--milestone|--human] [--needs o1,o2]
meguri outcome rm   <o>              # 両方向の requires 辺と serving Work も削除
meguri outcome done   <o>      # 人手で満たし表明(rollup 以外。run 抜きで消し込める=受理事実を貼る)
meguri outcome undone <o>      # 人手表明を取り消す
meguri work    add "<objective>" --for <o> [--by ai|human]
meguri work    ls   [--for <o>]
meguri work    edit <w> [--objective <s>] [--by ai|human]
meguri work    rm   <w>              # worktree/ブランチ/pane を掃除。DB 行は soft-delete(tombstone、id を再利用しない)
meguri run <o> [--agent <cmd>] [--wait] [--grace-secs N] [--timeout-secs N] [--nudge-secs N]
                              # o14-o16: ready Outcome → Work を起こし bare から隔離 worktree を切り、
                              #   その worktree の pane でエージェントを起動して実装プロンプトを注入(state=running)。
                              #   既定は detach(即返る。harvest は meguri watch)。--wait でその場でブロックして検証・gate まで。
                              #   注入落ち対策で(--wait 中は)result が出るまで --nudge-secs 間隔で再注入。attach 案内を表示
meguri accept <w>             # ローカル Human Gate: verified Work を受理 → serve 先 Outcome が satisfied → 後続が ready
meguri watch [--once] [--interval-secs N]
                              # 最小 reconciler: running Work を走査し、result が出ていれば harvest(検証→gate→Artifact)、
                              #   まだなら pane で沈黙 nudge。既定は running が捌けるまでループ、--once で 1 パス。
                              #   TTY では要約行を同じ行で上書き更新(harvest/nudge の行は残す)
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
~/.meguri/repos/<name>.git   管理 repo の bare clone(repo add で作る)
~/.meguri/worktrees/<repo>/w<id>/          Work の隔離 worktree(o14)
~/.meguri/worktrees/<repo>/w<id>/.meguri/prompt.md   実装プロンプト(o15、commit しない)
~/.meguri/worktrees/<repo>/w<id>/.meguri/result.json エージェントが書く完了結果(o16 で検知予定)
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

## 既知の割り切り(意図的な穴)

* `command` verify の Outcome は **verified な Work を `meguri accept` するまで** satisfied にならない(ローカル Human Gate)。前提が揃えば ready にはなる。
* サイクル防止は `add_requires` にあるが、現行 CLI(`outcome add --requires` は既存ノードのみ参照)では実際にサイクルを作れないため、防御は休眠状態。
* accept しても **worktree・pane は残る**(§3.5 で人間が引き取れるように意図的に。後片付けは別増分)。`work rm` で明示的に掃除する。
* 失敗経路(fix turn=o22 / 沈黙 nudge=o23 / timeout=o24 / pane 死亡=o25)は未実装。`rework`/`failed` になった Work は今は人間が引き取る。
