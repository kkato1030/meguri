# meguri（巡り）

*Read this in [English](README.md).*

**AI コーディングエージェントをループで走らせる — ターミナルマルチプレクサの中で。だから、いつでも人間が介入できる。**

meguri は [nexu-io/looper](https://github.com/nexu-io/looper) のアイデアの再実装ですが、アーキテクチャ上の意図的な違いが 1 つあります。ヘッドレスなワンショット実行（`claude --print …`）の代わりに、meguri は各エージェントを **[herdr](https://herdr.dev) または tmux の pane 内のライブな対話セッション**として実行します。オーケストレータがプロンプトを注入して結果を待つ間、あなたはいつでも pane にアタッチできます — 眺める、追加の指示を打ち込む、パーミッションダイアログに答える、完全に引き継ぐ — ループを壊すことなく。

```
GitHub issue (label: meguri:ready)
        │  discover & claim (+meguri:working)
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
git push + PR (Closes #N) — フェーズを meguri:implementing に差し替え
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

## セキュリティ

meguri の根本的なトレードオフは「監督なしの実行」です。使う前に理解しておく価値があります。

- **エージェントは本物のシェルアクセスを持ちます。** 既定の `[agent].args` には `--dangerously-skip-permissions` が入っており、ループが issue を拾った時点から、エージェントはその worktree 内で任意のコマンド（git、cargo、ネットワークアクセスなど、そのCLI が許すもの全て）を、コマンドごとの確認なしに実行できます。これが無人ループを可能にしている前提であり、裏を返せば「エージェントにその権限を与えても構わない環境」でしか meguri を動かすべきではない、ということです（使い捨ての VM やコンテナ、あるいは荒らされても許容できるマシン/アカウント）。コマンドごとにゲートしたい場合は `args = ["--permission-mode", "acceptEdits"]` を設定し（「[設定](#設定)」参照）、pane にアタッチしてダイアログに答えてください。
- **issue 本文はプロンプト入力です。** issue 本文全体（とループが読むコメント）はそのままエージェントのプロンプトに注入されます。誰でも issue を開けるリポジトリでは、悪意ある issue 本文はシェルアクセスを持つエージェントへの prompt injection の試みになり得ます。その緩和策が「[ラベルによるゲート](#ラベル)」です：ループは `meguri:*` フェーズラベル（`meguri:plan` / `meguri:ready`）が既に付いている issue にしか反応せず、ラベルを付与できるのは collaborator（write 権限）だけです。つまり「誰がエージェントを動かせるか」は「誰がこのリポジトリへの write 権限を持つか」に還元され、「誰が issue を開けるか」には依存しません。collaborator 権限を付与する際はこれを踏まえ、信頼できない issue に自分で `meguri:ready` を付けないでください。
- **完了判定は画面パースではなく独立検証です。** 上記「[完了契約（completion contract）](#完了契約completion-contract)」の通り、meguri はエージェント自身の「成功しました」という主張をそのまま信用しません。run を完了扱いする前に git の状態、base より先行するコミット、プロジェクトの `check_command` を再検証します。これは侵害された/誤誘導されたエージェントの被害範囲を（完全にではありませんが）限定します — run の最中に worktree 内で何かをすることはできても、result ファイルに「成功」と書くだけで meguri に不正な状態をマージさせることはできません。

meguri 自体の脆弱性を見つけた場合は [SECURITY.md](SECURITY.md) を参照してください。

## インストールとセットアップ

前提: `git`、[`gh`](https://cli.github.com)（認証済み）、エージェント CLI（デフォルトは `claude`）、そしてマルチプレクサ — 起動中の [herdr](https://herdr.dev)（推奨。エージェント状態のネイティブ検出）または `tmux`（画面ヒューリスティックのフォールバック）。これらのランタイム前提はインストール方法によらず同じです — 配布バイナリを使う場合もホストに `git`/`gh`/マルチプレクサが必要です。

```bash
cargo install --path .   # or: cargo build --release
meguri init              # writes ~/.meguri/config.toml, creates the db
meguri doctor            # checks gh auth, mux, agent CLI
```

バイナリの入手方法（その他）:

- **配布バイナリ** — [最新の GitHub Release](https://github.com/kkato1030/meguri/releases/latest) から自分のプラットフォーム（macOS arm64 / Linux x86_64）のアーカイブをダウンロードし、`.sha256` で検証・展開して `meguri` を `PATH` に置きます。
- **crates.io** — `cargo install meguri`（crate の publish 後。[ステータス / ロードマップ](#ステータス--ロードマップ) を参照）。

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
meguri top                # 稼働中の agent ペインを 1 タブにタイル表示するダッシュボード
meguri logs <run>         # event trail + live pane tail
meguri attach <issue>     # issue の agent pane に入る（run id も可）
meguri attach <issue> --review  # spec reviewer の独立 pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
```

### ローカルモード（GitHub もラベルも使わない）

ラベルを触れない/触りたくないリポジトリでは、プロジェクトを**完全に手元で**回せます。タスクキュー・claim・エスカレーション・完了判定は GitHub ラベルではなく meguri の sqlite に載り、成果物は PR ではなく検証済みのローカルブランチになります。`mode = "local"` を設定すると `repo_slug` は optional になり、`meguri doctor` も `gh` を要求しなくなります:

```toml
[[projects]]
id = "work"
repo_path = "/abs/path/to/repo"
mode = "local"          # "github"（デフォルト） | "local"
default_branch = "main"
check_command = "cargo test"
# deliver = "branch"    # local のデフォルト: 検証済みコミットをローカルブランチに残す（push も PR もしない）
```

ラベルの代わりにローカルタスクコマンドで投入・追跡します:

```bash
meguri add "export コマンドに --json フラグを足す"   # タスクを投入
meguri add --file task.md                            # 1 行目の見出し → title、本文 → body
meguri add --plan "export フォーマットを設計する"    # worker ではなく planner に投入
meguri tasks                                         # 未完了タスク一覧（needs_human は強調）
meguri watch                                         # poll 間隔以内に拾って走らせる
```

ローカル run は `meguri/t<task-id>-<slug>-<hash>` ブランチで作業し、成功すると検証済みコミットをそこに残して task を `done` にします（push はしません）。失敗した run は task を reason 付きの `needs_human` にし（`meguri tasks` / `meguri ps` で見えます）、次の run が再 claim して解除します。ブランチは自分で確認してマージしてください（`meguri review` / `accept` は後のフェーズで入ります）。

> **Phase 3 までは単一マシン前提。** ローカルモードではローカル sqlite が*唯一の真実*なので、1 リポジトリにつき meguri ホストは 1 台で回してください。共有キューに複数ホストを載せるのは Phase 4（lease 付きのリモート DB `TaskSource`）で、語彙と契約は [ADR 0003](docs/adr/0003-tasksource-task-moves-run-pins.md) で固定済みです。`silent` モード（issue を読むがラベルは書かない）・`deliver = "patch"`・`meguri review`/`accept`/`reject` も後のフェーズです。

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

meguri の issue ラベルは**2軸**で構成されます（[ADR 0005](docs/adr/0005-issue-labels-two-axis-phase-and-ball.md) 参照）。**軸1 — フェーズ**: meguri が関与した issue には、キューイングからクローズまで常にちょうど1つのフェーズラベルが付きます。**軸2 — ボールの所在**（誰の番か）: これはフェーズラベルを**剥がさず重ねて**付くので、「誰が詰まっているか」と「どこで詰まっているか」の両方が読み取れます。結果として、**無ラベルの issue は「未トリアージ」の一義的な意味**（人間が対応するか meguri に投げるかを判断する）になり、🔴 `meguri:needs-human` でフィルタすればそのまま人間の TODO 一覧になります。

**軸1 — フェーズ**（関与済み issue にちょうど1つ）:

| ラベル | 色 | 意味 |
|---|---|---|
| `meguri:plan` | 🔵 青 | planner ループにキューイング（オプトインの spec 先行フロー。あなたが付ける） |
| `meguri:speccing` | 🟣 紫 | spec PR が open（reviewing / ready の詳細は PR 側を見る） |
| `meguri:ready` | 🔵 青 | worker ループにキューイング（あなたが付ける、または spec 承認後に付く） |
| `meguri:implementing` | 🟢 緑 | 実装 PR が open（CI 修正・レビュー・マージ待ちを含む） |

**軸2 — ボールの所在**（フェーズに重ねて付く。全部なし = ループの次のポーリング待ち）:

| ラベル | 色 | 意味 |
|---|---|---|
| `meguri:working` | 🟡 黄 | エージェントがいままさに作業中（クレーム） |
| `meguri:needs-human` | 🔴 赤 | 人間が見る必要がある。理由はコメントで説明（フェーズラベルは残るので、*spec で詰まったのか実装で詰まったのか*が分かる） |
| `meguri:hold` | ⚪ 灰 | 人間が意図的に停止。discovery はスキップする |

加えて記録用 / オプトインのラベルが2つ: `meguri:clean-report` は cleaner ループのプロジェクト別レポート issue に付きます（`meguri:hold` を付けると巡回が止まる）。`meguri:automerge` は issue（worker が PR へコピーする）または PR に直接貼って GitHub ネイティブ auto-merge にオプトインします（下記「自動マージ(オプトイン)」参照）。

**PR 側**は現状維持です: spec PR は `meguri:spec-reviewing`（レビュー待ち）→ `meguri:spec-ready`（レビュー通過。実装を続ける）を持ちます — これらは PR に付き、issue のフェーズラベルとは独立です。CI 赤やマージ可否はラベルにミラーしません（GitHub がネイティブに表示する）。必要になれば `meguri:awaiting-merge` を PR ラベルとして後から足せます。

新しく作られる meguri ラベルは自動でスキームの色が付きます。このスキーム以前に（汎用の青で）作られてしまったラベルは、一度きり `gh label edit <name> --color <hex>`（例: `gh label edit meguri:implementing --color 0E8A16`）で色を是正してください — meguri は毎ポーリングで既存ラベルを recolor しないので、あなたが意図的に付けた色を上書きし続けることはありません。

discovery は GitHub ネイティブの issue dependencies（looper の ADR-0004）も尊重します: 他の issue に *blocked by* されている issue は、すべてのブロッカーが **completed** で close されるまでスキップされます — ラベルもコメントも付けない、静かなスキップです。*not planned* / *duplicate* で close されたブロッカーは解決扱いになりません（依存元 issue は人間の再検討待ち）。ブロッカーが読めない場合も「未解決」として扱われます。

### spec 先行フロー（オプトイン）

`meguri:ready` の代わりに `meguri:plan` を貼ると、**planner** ループがリポジトリを調査し、軽量な 1 ファイル `docs/specs/issue-<N>.md`（受け入れ条件・触るファイル・決定事項）だけを含む *spec PR*（`Spec: <title>`、`meguri:spec-reviewing` 付き）を開きます。続いて **spec reviewer** ループが spec PR をレビューします: 指摘があればサマリコメントとして投稿され（修正を push すると新しい head を再レビュー。同じ head は 1 回しかレビューされません）、指摘なしならラベルが `meguri:spec-ready` に貼り替わります — 人間が直接貼り替えても構いません。その後 worker が **同じブランチ・同じ PR の上で** 実装を続けます — spec と実装はまとめて 1 回でマージされます。spec 自体はレビュー用の使い捨ての足場で、spec worker が実装時に削除します — `docs/specs/` がデフォルトブランチに溜まっていくことはありません。残す価値のあるもの（設計判断・ドメイン規則）は ADR（`docs/adr/`）や永続的なドメイン文書へ振り分けられます。

### self-review（実装 diff の内部 AI レビュー）

実装 diff の AI レビューは **内部ループ**です（ADR 0006）: worker が PR を push する前に自分の diff をレビューするので、review→fix の往復は GitHub に一切触れません。`validate` と `open-pr` の間で worker は自分の worktree の中で self-review フェーズを回します — **review turn** が `git diff <base>...HEAD` をローカルで読んで `{verdict, findings[]}` を書き、findings があれば **fix turn** が潰して commit、プロジェクトの check を再実行し、review に戻ります。収束は forge マーカーではなく **ローカルのラウンドカウンタ**（`review.max_rounds`）で縛り、上限に達しても clean にならなければ **block せず** PR を公開します（人間の merge ゲートが最後の砦）。未収束のときだけ PR 本文にフッタ 1 行が付きます。投稿は一切なし: thread も comment もポーリングもありません — 人間には最初から自己レビュー済みの PR が届き、PR 会話は人間・外部レビュー専用の綺麗な場に保たれます。review turn は routing の `impl-reviewer` profile で走るので、fix を行う author とは別モデルにもできます。外部のレビュー bot を使っている場合は `review.enabled = false` で止められます。

AI が thread を作らなくなるので、**fixer** の discover は自然と人間・外部 bot の thread だけを拾います — GitHub をレビュー transport に使うのは「人間が居る側」に限定されます。

### cleaner（read-only のリポジトリ巡回）

**cleaner** ループは default branch の head を定期的に歩いて回り、蓄積した乖離 — spec と実装のずれ、dead code の候補、規約からの逸脱、置き去りの TODO、stale なリモートブランチ、孤児化した `meguri:working` ラベル — を `meguri:clean-report` ラベル付きの **1 本のレポート issue**（1 project = 1 issue）に書き留めます。修正は一切しません: 書き込みはこの issue の作成・更新だけで、push もブランチ操作も、他の issue / PR へのラベルやコメントもしません。本文は巡回のたびに完全に書き直されるスナップショットで、隠しマーカーの head sha により同じ head が二度走査されることはなく、head が進んでも `clean.interval_hours` を過ぎるまで次の巡回は走りません。検出項目を採用するなら通常の issue を切って `meguri:plan` / `meguri:ready` を付け、誤検知なら `clean.ignore` に部分文字列を足し、ループを止めたければレポート issue に `meguri:hold` を貼ってください。

GitHub 上のラベルとコメントが永続的なワークフロー状態です（looper の「Authority」原則）。ローカルの sqlite（`~/.meguri/meguri.sqlite`）は実行（run）の進行のみを追跡します。meguri はいつ kill しても構いません — `meguri watch` が復旧します: 生きている pane は再アダプトされ、死んだ run は最後にチェックポイントされたステップから再開されます。pane・claude session・worktree は **issue が寿命の単位**です — branch を編集するループ全員が共有する **author** pane が 1 枚（planner → worker/spec worker → fixer/ci fixer/conflict resolver が同じ live session で文脈を継ぐ）と、spec reviewer 専用の独立した **review** pane が 1 枚（さらに worker が self-review する間だけ一時的な **impl-review** pane）。turn が完了するたびにエージェントのネイティブ session id が issue の lane に保存されるので、idle 中に pane が死んでも次の run は同じ会話に `claude --resume <id>` で復帰します。watch 中は issue が close されると対応する pane・worktree・マージ済みローカルブランチが自動回収されます。一発実行運用では `meguri prune` で同じ掃除ができます。

ループ別の寿命の一覧:

| loop | trigger | 鍵 | worktree | 正常終了 | pane 後始末 |
|---|---|---|---|---|---|
| planner (author) | `meguri:plan` issue | issue | 新 branch | spec PR 作成 → `spec-reviewing` | keep |
| spec reviewer (review) | `spec-reviewing` PR / head 未レビュー | issue + `review` | read-only detached（`review-<issue>` 固定） | clean → `spec-ready` / findings → 据置 | keep（独立） |
| spec worker (author) | `spec-ready` PR | issue（branch 復元） | 既存 branch を継ぐ | 実装 → PR 更新 | keep・author pane を継ぐ |
| worker (author) | `meguri:ready` issue | issue | 新 branch | self-review → PR `Closes #N` | keep |
| fixer (author) | PR の未解決スレッド | issue（branch 復元） | PR head に attach | スレッドに再 review 依頼返信 | keep・author pane を継ぐ |
| ci fixer (author) | meguri PR の CI 赤 | issue（branch 復元） | PR head に attach | fix push（≤3 round） | keep・author pane を継ぐ |
| conflict resolver (author) | PR が Conflicting（≤3） | issue（branch 復元） | PR head に attach | base merge & 解消 → push | keep・author pane を継ぐ |
| cleaner (standalone) | レポート issue + 既定 branch 前進 | レポート issue | read-only detached | 単一レポート issue 再生成 | 自前回収 |

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
session = "meguri"     # ベースラベル。プロジェクトごとに専用 workspace
                       # `meguri:<project>`（herdr）/ `meguri-<project>`（tmux）を
                       # 使い、issue タブが混ざらないようにします。接尾辞なしの
                       # `meguri` は横断ビュー `meguri top` 用です。
# pane は issue 単位（author pane 1 枚 + review pane 1 枚）で保持され、issue が
# close されると回収されます。回収前にエージェントのネイティブ session id を保存
# （claude --resume <id>）。"never" は run 終了と同時に pane を閉じます（高速大量
# 運転向け）。それ以外の値は設定読込時にエラーになります。
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

[pr.auto_merge]        # GitHub ネイティブ auto-merge、オプトイン（上の「自動マージ」参照）
enabled = false
strategy = "squash"    # squash | merge | rebase
require_branch_protection = true
opt_in = "label"       # label | all

[clean]
interval_hours = 24     # cleaner の巡回間隔の下限（head が進んだだけでは走らない）
stale_branch_days = 30  # 最終コミットがこれより古いリモートブランチを stale として報告
ignore = []             # 誤検知を黙らせる部分文字列。プロジェクト単位は [projects.clean] で上書き

[review]
enabled = true    # worker の self-review フェーズ（実装 diff の内部 AI レビュー）のキルスイッチ
max_rounds = 3    # run ごとの self-review ラウンド上限。超えたら PR をそのまま公開する
# （旧 impl_enabled / impl_max_rounds キーも alias として読み込めます）
```

`[projects.pr]` は `[pr]` セクションを（キー単位ではなく)丸ごと上書きします: `[projects.pr]` を書いたプロジェクトは、省略したキーはデフォルトになり、`[pr.auto_merge]` も含めてそうなります。

## 開発

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

テストスイートは、スクリプト化された偽エージェント TUI（`tests/fixtures/fake_agent.sh`）を使い、本物の tmux・本物の git worktree・ローカルの bare origin に対してループ全体を駆動します — blocked ダイアログの処理、嘘をつくエージェントの矯正、検証フィードバック、クラッシュリカバリを含みます。

## ステータス / ロードマップ

GitHub 上で 8 つのループが動きます。looper のロールモデルを踏襲し、いずれも同じターンエンジンを共有する `Loop` 実装です: **worker**（issue → self-review → PR）、**planner**（`meguri:plan` issue → spec PR）、**spec reviewer**（`meguri:spec-reviewing` PR → サマリレビュー → `meguri:spec-ready`）、**spec worker**（`meguri:spec-ready` PR → 同じブランチ・同じ PR に実装コミットを積む）、**fixer**（meguri の PR の未解決レビューコメント → 修正コミットを push）、**ci fixer**（CI チェックが赤で確定した meguri の PR → 失敗ジョブのログを agent に渡す → 修正コミットを push。3 回の修正ラウンド後もまだ赤なら `meguri:needs-human` にエスカレーション）、**conflict resolver**（CONFLICTING な meguri の PR → ベースブランチを取り込み、コンフリクトを解消したマージコミットを push）、**cleaner**（定期的な read-only 巡回 → 乖離レポートを 1 本の `meguri:clean-report` issue に）。実装 diff の AI レビューはもうループではなく worker の内部フェーズ（**self-review**、ADR 0006）です: run の worktree の中で回り、forge には一切触れません。

**バージョニング。** meguri は 1.0 前（`0.x`）で [SemVer](https://semver.org/lang/ja/) に従います: `0.x` の間は public API と CLI が未安定で、minor（`0.y`）が破壊的変更を含みうる一方、patch（`0.y.z`）は互換を保ちます。安定を約束するのは `1.0.0` からです。現在の挙動に依存する場合はバージョンを固定してください。

**リリース。** リリースはタグ駆動です（ADR 0007）: メンテナがバージョンを bump し、`CHANGELOG.md` を更新して `vX.Y.Z` タグを push すると、`.github/workflows/release.yml` が macOS arm64 / Linux x86_64 のバイナリをビルドして GitHub Release に添付し（本文は git-cliff 生成のノート）、（crate 設定が済めば）OIDC Trusted Publishing で crates.io に publish します。**push したタグがそのままリリースの起点**なので、タグは慎重に — 誤タグは誤リリースになります。

## ライセンス

MIT
