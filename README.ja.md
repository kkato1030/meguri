# meguri（巡り）

*Read this in [English](README.md).*

**AI コーディングエージェントをループで走らせる — ターミナルマルチプレクサの中で。だから、いつでも人間が介入できる。**

meguri は [nexu-io/looper](https://github.com/nexu-io/looper) のアイデアの再実装ですが、アーキテクチャ上の意図的な違いが 1 つあります。ヘッドレスなワンショット実行（`claude --print …`）の代わりに、meguri は各エージェントを **[herdr](https://herdr.dev) または tmux の pane 内のライブな対話セッション**として実行します。オーケストレータがプロンプトを注入して結果を待つ間、あなたはいつでも pane にアタッチできます — 眺める、追加の指示を打ち込む、パーミッションダイアログに答える、完全に引き継ぐ — ループを壊すことなく。

```
GitHub issue (label: meguri:ready)
        │  discover & claim (meguri:working)
        ▼
git worktree (meguri/<issue>-<slug>-<hash>)
        │
        ▼
┌─ herdr / tmux pane ─────────────────┐
│ $ claude                            │   orchestrator: inject prompt,
│ > Read .meguri/prompt-….md and      │   wait for .meguri/result.json,
│   carry it out completely.          │   verify commits, run checks
│ ⏺ working…                          │
│                                     │◀─ you: attach anytime, type,
└─────────────────────────────────────┘   answer dialogs, take over
        │  verified commits + checks pass
        ▼
git push + PR (Closes #N) — labels settled
```

## なぜ対話セッションなのか？

ヘッドレスなループは不透明に失敗します。エージェントがパーミッションプロンプトに引っかかる、停止する、誤った方向に進む — 手元に残るのはログだけです。meguri ではエージェントの本物の TUI が常にそこにあります:

- **Blocked ≠ 失敗。** エージェントがパーミッション/質問ダイアログを表示すると、meguri はその実行に `awaiting_human` のフラグを立て、アタッチ方法を通知します — タイマーは止まり、何も kill されません。
- **人間の入力は決してエラーではない。** 実行の途中でアタッチして入力できます。オーケストレータは永続的なシグナル（result ファイル、git の状態、ラベル）のみに基づいて動くため、あなたの介入を許容し、吸収します。
- **沈黙はナッジされるだけで、罰せられない。** 静かなエージェントには上限回数までリマインダー行が送られ、その後は人間が呼ばれます。meguri は遅いという理由で実行を自動的に失敗させることはありません。
- **takeover / handback。** `meguri takeover <run>` でオーケストレータを待機させ、あなたが同じセッションを運転します。`meguri handback <run>` で、あなたの作業をコンテキストに含めたままループを再開します。

## 完了契約（completion contract）

meguri は成功判定のためにエージェントの画面をパースすることは決してありません。各ターンは worktree にプロンプトファイルを書き込み、最後に次のファイルを書いて終了するようエージェントに指示します:

```json
// .meguri/result.json
{"turn_id": "<uuid>", "status": "success | failure | needs_human", "summary": "…"}
```

古い turn id は無視されます。success を主張する結果は、meguri が次に進む前に**独立して検証**されます（クリーンなツリー、base ブランチより先行するコミット、プロジェクトの check コマンドの成功）。検証の失敗は修正ターンとしてエージェントに差し戻されます。

## インストールとセットアップ

前提: `git`、[`gh`](https://cli.github.com)（認証済み）、エージェント CLI（デフォルトは `claude`）、そしてマルチプレクサ — 起動中の [herdr](https://herdr.dev)（推奨。エージェント状態のネイティブ検出）または `tmux`（画面ヒューリスティックのフォールバック）。

```bash
cargo install --path .   # or: cargo build --release
meguri init              # writes ~/.meguri/config.toml, creates the db
meguri doctor            # checks gh auth, mux, agent CLI
```

`meguri init` は次のプロジェクトスタブ入りの最小 `~/.meguri/config.toml` を書き出すので、値を埋めます:

```toml
[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"
repo_slug = "owner/repo"
# default_branch = "main"
# check_command = "cargo test"   # 推奨: meguri 自身がこれを実行して検証します
```

それ以外はすべて任意です。既定値を上書きしたいセクション/キーだけを書きます（[設定](#設定) を参照）。

## 使い方

```bash
# one-shot: work a single issue
meguri run --project myproj --issue 42

# or keep watching: label an issue `meguri:ready` and meguri picks it up
meguri watch

meguri ps                 # runs, interaction state, panes
meguri logs <run>         # event trail + live pane tail
meguri serve              # 読み取り専用 web ダッシュボード (http://127.0.0.1:8607)
meguri attach <run>       # jump into the agent's pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri clean              # reclaim worktrees of closed issues (--dry-run / --force)
```

### web ダッシュボード

`meguri serve` で読み取り専用ダッシュボードが `http://127.0.0.1:8607` に立ちます（`--port` / `--bind` または config の `[server]` セクションで変更可）。`meguri ps` 相当の runs テーブル（`awaiting_human` の run を最上部で強調表示）に加え、run ごとの詳細ページでイベントトレイル、端末風のペインテール、turn 履歴、コピー可能な attach コマンドが見られます。同じ sqlite を読む独立プロセスなので `meguri watch` が動いていなくても使え、watch の生死は scheduler が tick ごとに書くハートビートから表示されます。認証はないためデフォルトは loopback バインドです（それ以外を指定すると警告が出ます）。

### ラベル

| ラベル | 意味 |
|---|---|
| `meguri:ready` | worker ループに issue をキューイングする（あなたが付ける） |
| `meguri:plan` | planner ループに issue をキューイングする（オプトインの spec 先行フロー） |
| `meguri:spec-reviewing` | spec PR に付く: reviewer ループ（または人間）のレビュー待ち |
| `meguri:spec-ready` | spec PR に付く: レビュー通過。worker が実装を続ける |
| `meguri:working` | meguri がクレーム済み（PR が開くと外れる） |
| `meguri:hold` | discovery がこの issue をスキップする |
| `meguri:needs-human` | meguri が断念。理由はコメントで説明される |

### spec 先行フロー（オプトイン）

`meguri:ready` の代わりに `meguri:plan` を貼ると、**planner** ループがリポジトリを調査し、軽量な 1 ファイル `docs/specs/issue-<N>.md`（受け入れ条件・触るファイル・決定事項）だけを含む *spec PR*（`Spec: <title>`、`meguri:spec-reviewing` 付き）を開きます。続いて **reviewer** ループが spec PR をレビューします: 指摘があればサマリコメントとして投稿され（修正を push すると新しい head を再レビュー。同じ head は 1 回しかレビューされません）、指摘なしならラベルが `meguri:spec-ready` に貼り替わります — 人間が直接貼り替えても構いません。その後 worker が **同じブランチ・同じ PR の上で** 実装を続けます — spec と実装はまとめて 1 回でマージされます。spec 自体はレビュー用の使い捨ての足場で、spec worker が実装時に削除します — `docs/specs/` がデフォルトブランチに溜まっていくことはありません。残す価値のあるもの（設計判断・ドメイン規則）は ADR（`docs/adr/`）や永続的なドメイン文書へ振り分けられます。

GitHub 上のラベルとコメントが永続的なワークフロー状態です（looper の「Authority」原則）。ローカルの sqlite（`~/.meguri/meguri.sqlite`）は実行（run）の進行のみを追跡します。meguri はいつ kill しても構いません — `meguri watch` が復旧します: 生きている pane は再アダプトされ、死んだ run は最後にチェックポイントされたステップから再開されます。 watch 中は issue が close されると対応する worktree（とマージ済みローカルブランチ）も自動回収されます。一発実行運用では `meguri clean` で同じ掃除ができます。

## 設定

すべての項目に既定値があるため、`config.toml` には `[[projects]]` と上書きしたい項目だけを書けば残りは既定値で埋まります — `meguri init` はその前提の最小テンプレートを書き出します。既定値の一覧:

```toml
# エージェントが書く成果物（PR 説明・summary・spec・レビュー）の言語。自由記述
# （例: "日本語", "English"）。省略するとエージェント任せ（通常は英語）。
# プロジェクト単位は [[projects]] 内の language で上書き。
language = "日本語"

[mux]
kind = "auto"          # auto | herdr | tmux
session = "meguri"     # herdr workspace label / tmux session name
keep_pane = "on-failure"  # also: always | never

[agent]
command = "claude"
# Default is yolo: the agent runs in an isolated worktree, and an autonomous
# loop stalls if it asks permission for every git/cargo command. To gate each
# command instead, set args = ["--permission-mode", "acceptEdits"] and answer
# dialogs by attaching to the pane.
args = ["--dangerously-skip-permissions"]

[limits]
idle_grace_secs = 90        # silence before a nudge
nudge_limit = 2             # nudges before paging a human
max_turn_runtime_secs = 2700
result_grace_secs = 60      # wait for Working→Idle after result appears
validate_turns = 3          # fix attempts for a failing check_command

[scheduler]
poll_interval_secs = 60
max_concurrent_runs = 2

[server]
port = 8607            # meguri serve のリッスンポート
bind = "127.0.0.1"     # 認証なしのため loopback 推奨

[pr]
draft = true   # PR をドラフトで作成。プロジェクト単位は [projects.pr] で上書き
```

## 開発

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

テストスイートは、スクリプト化された偽エージェント TUI（`tests/fixtures/fake_agent.sh`）を使い、本物の tmux・本物の git worktree・ローカルの bare origin に対してループ全体を駆動します — blocked ダイアログの処理、嘘をつくエージェントの矯正、検証フィードバック、クラッシュリカバリを含みます。

## ステータス / ロードマップ

GitHub 上で 5 つのループが動きます。looper のロールモデルを踏襲し、いずれも同じターンエンジンを共有する `Loop` 実装です: **worker**（issue → PR）、**planner**（`meguri:plan` issue → spec PR）、**reviewer**（`meguri:spec-reviewing` PR → サマリレビュー → `meguri:spec-ready`）、**spec worker**（`meguri:spec-ready` PR → 同じブランチ・同じ PR に実装コミットを積む）、**fixer**（meguri の PR の未解決レビューコメント → 修正コミットを push）。

## ライセンス

MIT
