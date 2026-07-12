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
meguri prune              # reclaim worktrees of closed issues (--dry-run / --force)
```

### 常駐させる（daemon）

`meguri watch` はフォアグラウンドに留まります。シェルを閉じても回り続けさせるには detach します:

```bash
meguri daemon start       # watch を detach 起動（ログ: ~/.meguri/logs/watch.log）
meguri daemon status      # pid / モード / 稼働状態 / ログ位置 / アクティブ run 数
meguri daemon logs -f     # daemon ログを follow
meguri daemon restart
meguri daemon stop        # SIGTERM。kill-safe なので次回起動時の recovery が再開する
```

macOS では監視を launchd に委ねると、ログアウト・再起動・クラッシュ後も自動復帰します:

```bash
meguri daemon install --mode launchd   # user LaunchAgent を生成して bootstrap
meguri daemon uninstall                # bootout + plist 削除
```

LaunchAgent には install 時の `PATH`（および設定されていれば `HERDR_SOCKET_PATH` /
`MEGURI_HOME`）が焼き込まれるため、launchd 配下でも `gh`・`tmux`/`herdr`・エージェント
CLI が解決できます。ログは `~/.meguri/logs/launchd.log` へ。restart policy と throttle は
config の `[daemon]` セクションから来ます — 変更したら `meguri daemon install` を再実行して
反映します。非対応プラットフォームでは明示エラーになります（silent fallback しません）。
systemd user unit は後続予定です。

どのモードでも watch プロセスは排他ロック（`~/.meguri/daemon/watch.lock`）を保持するので、
2 つ目のスケジューラ — フォアグラウンドでも detached でも — は二重駆動せず明示エラーで
落ちます。

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
| `meguri:automerge` | GitHub ネイティブ auto-merge にオプトインする（issue にも PR にも貼れる） |

discovery は GitHub ネイティブの issue dependencies（looper の ADR-0004）も尊重します: 他の issue に *blocked by* されている issue は、すべてのブロッカーが **completed** で close されるまでスキップされます — ラベルもコメントも付けない、静かなスキップです。*not planned* / *duplicate* で close されたブロッカーは解決扱いになりません（依存元 issue は人間の再検討待ち）。ブロッカーが読めない場合も「未解決」として扱われます。

### spec 先行フロー（オプトイン）

`meguri:ready` の代わりに `meguri:plan` を貼ると、**planner** ループがリポジトリを調査し、軽量な 1 ファイル `docs/specs/issue-<N>.md`（受け入れ条件・触るファイル・決定事項）だけを含む *spec PR*（`Spec: <title>`、`meguri:spec-reviewing` 付き）を開きます。続いて **reviewer** ループが spec PR をレビューします: 指摘があればサマリコメントとして投稿され（修正を push すると新しい head を再レビュー。同じ head は 1 回しかレビューされません）、指摘なしならラベルが `meguri:spec-ready` に貼り替わります — 人間が直接貼り替えても構いません。その後 worker が **同じブランチ・同じ PR の上で** 実装を続けます — spec と実装はまとめて 1 回でマージされます。

GitHub 上のラベルとコメントが永続的なワークフロー状態です（looper の「Authority」原則）。ローカルの sqlite（`~/.meguri/meguri.sqlite`）は実行（run）の進行のみを追跡します。meguri はいつ kill しても構いません — `meguri watch` が復旧します: 生きている pane は再アダプトされ、死んだ run は最後にチェックポイントされたステップから再開されます。 watch 中は issue が close されると対応する worktree（とマージ済みローカルブランチ）も自動回収されます。一発実行運用では `meguri prune` で同じ掃除ができます。

### 自動マージ（オプトイン）

meguri は「マージして安全か」を自前で判定しません — 条件の揃った PR に GitHub ネイティブの auto-merge を arm する（`gh pr merge --auto`）だけで、いつマージするかの最終判断は GitHub（branch protection + required checks）に委ねます（`docs/adr/0003-auto-merge-github-native-arm-only.md` 参照）。デフォルトは無効で、二段のオプトインでゲートします: マスタースイッチ `[pr.auto_merge].enabled` と、（`opt_in = "all"` でない限り）`meguri:automerge` ラベルです。ラベルを *issue* に貼ると worker が PR へコピーします（その PR は最初から non-draft で開きます）。PR に直接貼っても効きます。

watch のポーリングに相乗りする sweep が、**すべて**満たした PR を arm します: `meguri/` ブランチで `Closes #N.` により issue に紐づいている / `meguri:hold`・`meguri:needs-human`・`meguri:working`・`meguri:spec-reviewing`・`meguri:spec-ready` のいずれも付いていない（spec フェーズ中は絶対に arm しない）/ 未解決 review thread がゼロ / リポジトリが auto-merge と設定した strategy を許可している（必要なら required checks 付き branch protection もある）。arm はレビュー済み head に `--match-head-commit` で固定され、マーカーコメント（`<!-- meguri:automerge armed head=<sha> -->`）が冪等性と人間の上書き尊重を担います — 人間が後で auto-merge を解除した head は再 arm しません（新しい push で再判定）。arm しようとした時点で GitHub が既に「マージ可能」と判定していた場合は、meguri がその判定に従ってマージを確定します。

```toml
[pr.auto_merge]
enabled = false                  # マスタースイッチ
strategy = "squash"              # squash | merge | rebase(リポジトリで不許可なら fallback せず拒否)
require_branch_protection = true # required checks 付き protection がなければ arm しない
opt_in = "label"                 # label(meguri:automerge が必要) | all(全 meguri PR が対象)
```

`enabled = true` なのにリポジトリが auto-merge を honor できない（auto-merge 不許可・strategy 不許可・protection なし）場合、`meguri watch` 起動時と `meguri doctor` で **fail-fast** します（マージ時に静かに劣化させない）。逃げ道は同じ `require_branch_protection = false` で、注意点が二つ: protection 検出は **classic branch protection API のみ**（rulesets は検出できない）で、その参照には **admin 権限のトークン**が必要です（admin でないトークンは HTTP 403 になり、meguri はそれを「protection なし」に倒さずエラーとして返します）。また auto-merge 3/3 まではレビューギャップがあります: meguri 自身のレビューが clean であることを arm 条件に足す reviewer ゲート（`require_clean_review`）は後段の issue で入るため、それまではオプトイン PR が meguri のレビュー前でも required checks さえ通れば merge され得ます — 求める品質バーは branch protection 側で担保してください。

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

[daemon]
restart_policy = "on-failure"  # launchd KeepAlive: never | on-failure | always
throttle_secs = 10             # launchd ThrottleInterval（再起動の最短間隔・秒）

[server]
port = 8607            # meguri serve のリッスンポート
bind = "127.0.0.1"     # 認証なしのため loopback 推奨

[pr]
draft = true   # PR をドラフトで作成。プロジェクト単位は [projects.pr] で上書き

[pr.auto_merge]        # GitHub ネイティブ auto-merge、オプトイン（上の「自動マージ」参照）
enabled = false
strategy = "squash"    # squash | merge | rebase
require_branch_protection = true
opt_in = "label"       # label | all
```

`[projects.pr]` は `[pr]` セクションを（キー単位ではなく)丸ごと上書きします: `[projects.pr]` を書いたプロジェクトは、省略したキーはデフォルトになり、`[pr.auto_merge]` も含めてそうなります。

## 開発

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

テストスイートは、スクリプト化された偽エージェント TUI（`tests/fixtures/fake_agent.sh`）を使い、本物の tmux・本物の git worktree・ローカルの bare origin に対してループ全体を駆動します — blocked ダイアログの処理、嘘をつくエージェントの矯正、検証フィードバック、クラッシュリカバリを含みます。

## ステータス / ロードマップ

GitHub 上で 6 つのループが動きます。looper のロールモデルを踏襲し、いずれも同じターンエンジンを共有する `Loop` 実装です: **worker**（issue → PR）、**planner**（`meguri:plan` issue → spec PR）、**reviewer**（`meguri:spec-reviewing` PR → サマリレビュー → `meguri:spec-ready`）、**spec worker**（`meguri:spec-ready` PR → 同じブランチ・同じ PR に実装コミットを積む）、**fixer**（meguri の PR の未解決レビューコメント → 修正コミットを push）、**conflict resolver**（CONFLICTING な meguri の PR → ベースブランチを取り込み、コンフリクトを解消したマージコミットを push）。

## ライセンス

MIT
