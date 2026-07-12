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
meguri attach <run>       # jump into the agent's pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
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
| `meguri:clean-report` | cleaner ループのプロジェクト別レポート issue（`meguri:hold` を付けると巡回が止まる） |

discovery は GitHub ネイティブの issue dependencies（looper の ADR-0004）も尊重します: 他の issue に *blocked by* されている issue は、すべてのブロッカーが **completed** で close されるまでスキップされます — ラベルもコメントも付けない、静かなスキップです。*not planned* / *duplicate* で close されたブロッカーは解決扱いになりません（依存元 issue は人間の再検討待ち）。ブロッカーが読めない場合も「未解決」として扱われます。

### spec 先行フロー（オプトイン）

`meguri:ready` の代わりに `meguri:plan` を貼ると、**planner** ループがリポジトリを調査し、軽量な 1 ファイル `docs/specs/issue-<N>.md`（受け入れ条件・触るファイル・決定事項）だけを含む *spec PR*（`Spec: <title>`、`meguri:spec-reviewing` 付き）を開きます。続いて **reviewer** ループが spec PR をレビューします: 指摘があればサマリコメントとして投稿され（修正を push すると新しい head を再レビュー。同じ head は 1 回しかレビューされません）、指摘なしならラベルが `meguri:spec-ready` に貼り替わります — 人間が直接貼り替えても構いません。その後 worker が **同じブランチ・同じ PR の上で** 実装を続けます — spec と実装はまとめて 1 回でマージされます。spec 自体はレビュー用の使い捨ての足場で、spec worker が実装時に削除します — `docs/specs/` がデフォルトブランチに溜まっていくことはありません。残す価値のあるもの（設計判断・ドメイン規則）は ADR（`docs/adr/`）や永続的なドメイン文書へ振り分けられます。

### 実装レビュー（実装 diff の AI レビュー）

meguri の AI レビューは **spec PR と実装 diff の両方** を対象にします。meguri の実装 PR が静かになったら — CI が green、spec 系ラベルなし、fixer 待ちのレビュースレッドなし — **impl reviewer** ループが head を read-only でチェックアウトして diff をレビューします: 指摘は **inline のレビュースレッド**（+ マーカー入りサマリコメント）として投稿されます。これはまさに fixer の入力そのものなので、既存の review→fix の往復（ping-pong）が新しい機構なしでそのまま拾います。指摘なしならマーカー入りコメントだけが投稿され、何も反応しません。このループはラベルレスで、収束は三重に栓がされています（ADR 0004）: 同じ head は 1 回しかレビューされない（PR 上の隠し head-sha マーカー）、PR ごとのラウンド数に上限がある（`review.impl_max_rounds`）、そして clean 判定はスレッドを作りません。AI は approve も request-changes も決してしません — レビューは常に COMMENT のみで、**マージは人間の判断のまま**です。外部のレビュー bot を使っている場合は `review.impl_enabled = false` で止められます。

### cleaner（read-only のリポジトリ巡回）

**cleaner** ループは default branch の head を定期的に歩いて回り、蓄積した乖離 — spec と実装のずれ、dead code の候補、規約からの逸脱、置き去りの TODO、stale なリモートブランチ、孤児化した `meguri:working` ラベル — を `meguri:clean-report` ラベル付きの **1 本のレポート issue**（1 project = 1 issue）に書き留めます。修正は一切しません: 書き込みはこの issue の作成・更新だけで、push もブランチ操作も、他の issue / PR へのラベルやコメントもしません。本文は巡回のたびに完全に書き直されるスナップショットで、隠しマーカーの head sha により同じ head が二度走査されることはなく、head が進んでも `clean.interval_hours` を過ぎるまで次の巡回は走りません。検出項目を採用するなら通常の issue を切って `meguri:plan` / `meguri:ready` を付け、誤検知なら `clean.ignore` に部分文字列を足し、ループを止めたければレポート issue に `meguri:hold` を貼ってください。

GitHub 上のラベルとコメントが永続的なワークフロー状態です（looper の「Authority」原則）。ローカルの sqlite（`~/.meguri/meguri.sqlite`）は実行（run）の進行のみを追跡します。meguri はいつ kill しても構いません — `meguri watch` が復旧します: 生きている pane は再アダプトされ、死んだ run は最後にチェックポイントされたステップから再開されます。 pane と worktree は issue 単位で生きます（1 issue = 1 pane。同じ issue の後続 run は生きている session を再利用）: watch 中は issue が close されると対応する pane・worktree・マージ済みローカルブランチが自動回収されます。回収前にエージェントのネイティブ session id が保存されるので、`claude --resume <id>` で文脈ごと復帰できます。一発実行運用では `meguri prune` で同じ掃除ができます。

## 設定

すべての項目に既定値があるため、`config.toml` には `[[projects]]` と上書きしたい項目だけを書けば残りは既定値で埋まります — `meguri init` はその前提の最小テンプレートを書き出します。

`meguri watch` はポーリングのたびに `config.toml` を読み直すので、編集はそれ以降に spawn される run から反映されます — デーモン再起動は不要です（実行中の run は開始時点の設定を保持します）。不正な編集（TOML の構文エラー、projects が空）はログに警告を出して拒否され、直前の有効な設定で動き続けます。プロセスの寿命に紐づく 2 つだけは例外で、再起動が必要です（ログでもその旨を警告します）: `mux.kind` / `mux.session`（`meguri watch` を再起動）と `[daemon]` セクション（`meguri daemon install` を再実行）。

既定値の一覧:

```toml
# エージェントが書く成果物（PR 説明・summary・spec・レビュー）の言語。自由記述
# （例: "日本語", "English"）。省略するとエージェント任せ（通常は英語）。
# プロジェクト単位は [[projects]] 内の language で上書き。
language = "日本語"

[mux]
kind = "auto"          # auto | herdr | tmux
session = "meguri"     # herdr workspace label / tmux session name
# pane は issue 単位（1 issue = 1 pane）で保持され、issue が close されると回収
# されます。回収前にエージェントのネイティブ session id を保存（claude --resume <id>）。
# "never" は run 終了と同時に pane を閉じます（高速大量運転向け）。
keep_pane = "until-issue-closed"  # also: never

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

[notifications]
macos = true           # awaiting_human を macOS 通知 (osascript) で知らせる
# webhook_url = "https://example.com/hook"  # JSON POST: run id / issue / reason / attach
throttle_secs = 60     # 同一 run の連続通知の最短間隔(秒)

[pr]
draft = true   # PR をドラフトで作成。プロジェクト単位は [projects.pr] で上書き

[clean]
interval_hours = 24     # cleaner の巡回間隔の下限（head が進んだだけでは走らない）
stale_branch_days = 30  # 最終コミットがこれより古いリモートブランチを stale として報告
ignore = []             # 誤検知を黙らせる部分文字列。プロジェクト単位は [projects.clean] で上書き

[review]
impl_enabled = true    # impl-reviewer ループ（実装 PR の AI レビュー）のキルスイッチ
impl_max_rounds = 3    # PR ごとの impl レビューのラウンド上限。超えたら人間に委ねる
```

## 開発

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

テストスイートは、スクリプト化された偽エージェント TUI（`tests/fixtures/fake_agent.sh`）を使い、本物の tmux・本物の git worktree・ローカルの bare origin に対してループ全体を駆動します — blocked ダイアログの処理、嘘をつくエージェントの矯正、検証フィードバック、クラッシュリカバリを含みます。

## ステータス / ロードマップ

GitHub 上で 9 つのループが動きます。looper のロールモデルを踏襲し、いずれも同じターンエンジンを共有する `Loop` 実装です: **worker**（issue → PR）、**planner**（`meguri:plan` issue → spec PR）、**reviewer**（`meguri:spec-reviewing` PR → サマリレビュー → `meguri:spec-ready`）、**spec worker**（`meguri:spec-ready` PR → 同じブランチ・同じ PR に実装コミットを積む）、**impl reviewer**（静かで green な meguri の実装 PR → inline レビュースレッドとして AI レビューを投稿し fixer に流す）、**fixer**（meguri の PR の未解決レビューコメント → 修正コミットを push）、**ci fixer**（CI チェックが赤で確定した meguri の PR → 失敗ジョブのログを agent に渡す → 修正コミットを push。3 回の修正ラウンド後もまだ赤なら `meguri:needs-human` にエスカレーション）、**conflict resolver**（CONFLICTING な meguri の PR → ベースブランチを取り込み、コンフリクトを解消したマージコミットを push）、**cleaner**（定期的な read-only 巡回 → 乖離レポートを 1 本の `meguri:clean-report` issue に）。

## ライセンス

MIT
