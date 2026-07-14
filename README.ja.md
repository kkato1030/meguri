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

対応プラットフォーム: meguri のコア（CLI・`watch`・全ループ）は macOS / Linux で動作します。`meguri daemon install`（`launchd` supervisor、「[常駐させる（daemon）](#常駐させるdaemon)」参照）は macOS 専用です。

```bash
cargo install --path .            # or: cargo build --release
meguri init                       # ~/.meguri/config.toml を作成（プロジェクトは 0 件）、db も作成
meguri add-project owner/repo     # [[projects]] を追記し、clone を実体化
meguri doctor                     # gh 認証・mux・agent CLI を検査
```

バイナリの入手方法（その他）:

- **配布バイナリ** — [最新の GitHub Release](https://github.com/kkato1030/meguri/releases/latest) から自分のプラットフォーム（macOS arm64 / Linux x86_64）のアーカイブをダウンロードし、`.sha256` で検証・展開して `meguri` を `PATH` に置きます。
- **crates.io** — `cargo install meguri`（crate の publish 後。[ステータス / ロードマップ](#ステータス--ロードマップ) を参照）。

**プロジェクト追加は 1 コマンド。** `meguri init` は **プロジェクト 0 件**の最小 `~/.meguri/config.toml` を書きます（`[[projects]]` スタブはコメントアウト済み）。追加は `meguri add-project` で行います — `[[projects]]` を追記し（コメントや手編集は保持）、clone を実体化し、環境検査をその場で流します:

```bash
meguri add-project owner/repo              # 既存の GitHub repo
meguri add-project owner/repo --create     # repo 新規作成から（gh repo create、初期コミット込み）
meguri add-project owner/repo --id myproj  # 導出される id を上書き（既定: repo 名）
meguri add-project --local /abs/path       # local mode プロジェクト（GitHub 不要。下記参照）
```

`--create` は初期コミット付きの実 GitHub repo を作る（= default branch が即座に存在する）ため、**自動ロールバックはできません** — meguri は自分が作った repo を削除しません。既定は private で、`--public` で public になります。`[[projects]]` を手書きしても構いません（`add-project` が書くのは普通のエントリです）:

```toml
[[projects]]
id = "myproj"
repo_slug = "owner/repo"
# repo_path = "/abs/path/to/clone"  # 省略すると meguri が clone を管理します（下記参照）
# default_branch = "main"
# check_command = "cargo test"   # 推奨: meguri 自身がこれを実行して検証します
```

それ以外はすべて任意です。既定値を上書きしたいセクション/キーだけを書きます（[設定](#設定) を参照）。

**managed clone（管理 clone）。** `repo_path` を省略すると、meguri が `repo_slug` から `~/.meguri/repos/<id>` に **bare** clone を実体化して所有します（`gh` 経由で認証を継承）。slug を宣言すれば clone は meguri が面倒を見ます。置き場所は `~/.meguri/worktrees` の外で、checkout は持たず、次の `watch`/`run` で(無ければ)作られます。`meguri doctor` は各プロジェクトを *clone 済み* / *未 clone* / *壊れている* で表示し、push できない `gh` トークンも検出します。自分で維持する clone を使いたいときは `repo_path` を明示します（従来どおり。meguri が上書き clone することはありません)。なお managed clone では、`worktree_setup` が secrets（`.env` や `.claude/settings.local.json`）を `cp` する元の working copy が無いので、host 側の供給源から渡してください。**local mode は従来どおり `repo_path` 必須**です（clone 元の `repo_slug` が無いため)。

### コーディングエージェントに meguri を勧めさせる

meguri は Claude Code の **skill** を同梱しています。これにより、コーディングエージェントが「このリポジトリは meguri が向いている」と気づき、無人シェル実行のトレードオフを最初に開示したうえで導入を提案できます（[ADR 0009](docs/adr/0009-agent-skill-distribution-symptom-trigger-honest-pitch.md) / [ADR 0012](docs/adr/0012-acquisition-skill-as-apm-subpath-github-ref.md)）。リポジトリで meguri が既に動いているかで、配布は 2 チャネルに分かれます:

- **まだ meguri を使っていない** — [apm](https://github.com/microsoft/apm) で skill を**ユーザーレベル**に入れます。こうすると、meguri を一度も見たことのないリポジトリでもエージェントが提案できます:

  ```bash
  # vX.Y.Z は最新リリースタグに置き換える: https://github.com/kkato1030/meguri/releases/latest
  apm install -g --target claude kkato1030/meguri/skills/meguri#vX.Y.Z
  ```

  `--target claude` は省略できません。省略すると apm は `~/.agents/skills/` にしか展開せず、Claude Code はそこを読まないため skill が発火しません。参照は必ずリリースタグ（`#vX.Y.Z`）にピンしてください — ピンしない参照は `main` に追従してドリフトします。

- **すでに meguri が動いている** — 定着側の対になるコマンドは `meguri agent-skills install` です。同じ埋め込みソース(`skills/meguri/`)を使うので、導入される内容は使っている `meguri` のビルドと必ず一致します:

  ```bash
  meguri agent-skills install            # ~/.claude/skills/meguri/ — 上と同じ skill を、この
                                          # バイナリの内容で更新(現状 --target はこれのみ)
  meguri agent-skills install --project  # カレントリポジトリの .claude/rules/meguri.md —
                                          # meguri 導入済みリポジトリの日常運用ルール。
                                          # 再実行しても安全(冪等)
  meguri agent-skills status             # 導入済みか・このバイナリ内蔵版と一致するか
  ```

  `meguri init` の完了時にはユーザーレベルの導入を対話で案内します。どちらのコマンドも、手で編集した
  ファイルを黙って上書きしません — 差分を提示し `--force` を求めます。

## 使い方

```bash
# capture: 一言メモから issue を立てる（あとで AI が整形する）
meguri add "ログイン後のリダイレクトが変"

# one-shot: work a single issue
meguri run --project myproj --issue 42

# or keep watching: label an issue `meguri:ready` and meguri picks it up
meguri watch

meguri ps                 # runs, interaction state, panes
meguri schedules          # cron スケジュール: 定義・最終発火・次回発火
meguri top                # 稼働中の agent ペインを 1 タブにタイル表示するダッシュボード
meguri logs <run>         # event trail + live pane tail
meguri attach <issue>     # issue の agent pane に入る（run id も可）
meguri attach <issue> --review  # pr-reviewer の独立 pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
```

### 投入口（`meguri add`）

最初に詰まるのは作業を投入するところです。`meguri add "<一言>"` はそれを
1 コマンドに下げます。挙動はプロジェクトのモードで変わります。

**github モード** — issue を即座に作り（`create_issue` 直で、LLM を通しません）、
番号と URL を出します。そのあと best-effort で headless の agent がリポジトリを
読み、タイトルと本文を整えます。原文メモは必ず末尾に verbatim で残るので、整形は
足場にすぎず、オーサリングの主権は原文にあります。投入は AI を待たず、AI で
失敗しません — agent が無い・整形が失敗・Ctrl-C のいずれでも raw の issue は
残ります。既定は無ラベル = 未トリアージ（watch は拾いません）。あとで
`meguri:plan` / `meguri:ready` を貼るか、`--plan` / `--ready` で即投入します。
`--raw` は整形を丸ごと省きます。整形は既定の `claude` CLI ならゼロ設定で動き、
`command` を別 CLI に替えるならそのプロファイルの `headless_args` も設定して
ください（未設定なら整形はスキップ・raw のまま、`meguri doctor` が指摘します）。

**local モード** — issue の代わりに sqlite にタスクを投入します（下記）。
`--file` は markdown のタスクを読み、`--not-before` は指定時刻まで保留します。
`--plan` は拒否されます: local モードにはまだ planner が無く（issue #54）、
plan タスクは誰にも拾われないためです。planner 仕事は github モードの
プロジェクトで使ってください。

`--project` は cwd（その `repo_path` 配下）から推定します。曖昧なら明示して
ください。

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
meguri tasks                                         # 未完了タスク一覧（needs_human は強調）
meguri watch                                         # poll 間隔以内に拾って走らせる
```

（`meguri add --plan` は github モード専用です。local モードにはまだ planner がありません — issue #54。）

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

加えて記録用 / オプトインのラベル: `meguri:clean-report` と `meguri:triage-report` は、それぞれ cleaner ループと triage ループのプロジェクト別レポート issue に付きます（どちらかに `meguri:hold` を付けると、その巡回が止まる）。`meguri:automerge` は issue（worker が PR へコピーする）または PR に直接貼って GitHub ネイティブ auto-merge にオプトインします（下記「自動マージ(オプトイン)」参照）。

**PR 側**は現状維持です: spec PR は `meguri:spec-reviewing`（レビュー待ち）→ `meguri:spec-ready`（レビュー通過。実装を続ける）を持ちます — これらは PR に付き、issue のフェーズラベルとは独立です。CI 赤やマージ可否はラベルにミラーしません（GitHub がネイティブに表示する）。必要になれば `meguri:awaiting-merge` を PR ラベルとして後から足せます。

新しく作られる meguri ラベルは自動でスキームの色が付きます。このスキーム以前に（汎用の青で）作られてしまったラベルは、一度きり `gh label edit <name> --color <hex>`（例: `gh label edit meguri:implementing --color 0E8A16`）で色を是正してください — meguri は毎ポーリングで既存ラベルを recolor しないので、あなたが意図的に付けた色を上書きし続けることはありません。

discovery は GitHub ネイティブの issue dependencies（looper の ADR-0004）も尊重します: 他の issue に *blocked by* されている issue は、すべてのブロッカーが **completed** で close されるまでスキップされます — ラベルもコメントも付けない、静かなスキップです。*not planned* / *duplicate* で close されたブロッカーは解決扱いになりません（依存元 issue は人間の再検討待ち）。ブロッカーが読めない場合も「未解決」として扱われます。

### spec 先行フロー（オプトイン）

`meguri:ready` の代わりに `meguri:plan` を貼ると、**planner** ループがリポジトリを調査し、軽量な 1 ファイル `docs/specs/issue-<N>.md`（受け入れ条件・触るファイル・決定事項）だけを含む *spec PR*（`Spec: <title>`、`meguri:spec-reviewing` 付き）を開きます。spec の深さは **適応的** です（[ADR 0010](docs/adr/0010-adaptive-spec-depth.md)）: planner は不確実性 × 影響範囲で `normal` かより深い `design` かを選び、永続状態や公開 contract に触れる変更は veto によって migration・rollback セクションを持つ深い構成になります — 選んだ深さの理由は spec または PR に残ります。任意の **pr-reviewer** レビュー（後述）が spec PR をレビューし、指摘なしならラベルを `meguri:spec-ready` に貼り替えます — 人間が直接貼り替えても構いません。その後は `plan_delivery` で分岐します（ADR 0008）:

- **`separate`**（既定）— 2 本の PR。spec/ADR PR は単体でレビュー・**マージ**されます（issue は非クローズの `Refs #N` で参照するので、マージしても issue は閉じません）。マージ済みの spec PR は issue を `speccing → ready` に張り替え、**worker** が着地した spec を読み込み（実装の一部として削除しつつ）別 PR で実装します。
- **`combined`** — 1 本の PR。**spec worker** が spec PR のブランチを takeover して実装を積みます（#98 の morph 型）。spec と実装はまとめて 1 回でマージされます。

いずれの場合も spec 自体はレビュー用の使い捨ての足場で、実装時に削除されます — `docs/specs/` がデフォルトブランチに溜まっていくことはありません。残す価値のあるもの（設計判断・ドメイン規則）は ADR（`docs/adr/`）や永続的なドメイン文書へ振り分けられます。

**分解提案**（[ADR 0016](docs/adr/0016-decompose-through-spec-review-gate-then-materialize.md)）— 1 つの spec に収まらない大きな issue（独立した PR としてレビュー・ロールバックしたい複数の成果物）だと planner が判断したときは、実装 spec の代わりに **分解提案 spec** を書きます: 親のゴール・要求カバレッジ表・子 issue の一覧と依存グラフ・rollout 順に加えて、機械可読な ` ```json meguri-children ` ブロックを 1 つ載せます。これは通常の spec と **同じ spec-review ゲート** を通ります（切り方と「どの子がどの親要求を満たすか」が、何かを起票する前にレビューされます）。提案 PR が承認されると（pr-reviewer が実際にレビューした head の `spec-ready`）、軽量な **materializer** sweep が子 issue を起こし、GitHub ネイティブの `blocked_by` 依存を張り、各子に指定のフェーズラベル（`meguri:ready` / `meguri:plan`、`human` ステップは無ラベル）を付け、親を無ラベルの **tracking** issue にして、使い捨ての提案 PR を未マージで閉じます（子 issue 群 + 依存が永続状態で、あとは discovery の既存の依存ゲートが rollout を順序づけます）。materialization は冪等で、途中まで進んだ実行は親の依存グラフから再開され、重複 issue は決して作りません。承認済みの提案を人間の判断まで保留したいときは `decompose.materialize_enabled = false`。分解は 1 レベルのみです（子はさらに分解できません）。

### レビュー: 内部 self-review（必須）+ GitHub pr-reviewer（任意）

spec と impl は対称です（ADR 0008）: どちらも PR を開く前に **必須の内部 self-review** を回し、開いた PR に対して **任意の外部 pr-reviewer** を有効化できます。

**内部 self-review** は **内部フェーズ**です（ADR 0006）: 作者が PR を push する前に自分の成果をレビューするので、review→fix の往復は GitHub に一切触れません。`validate` と `open-pr` の間で **review turn** がローカル diff を読んで `{verdict, findings[]}` を書き、設定した全レンズ（`review.lenses`、既定 `correctness / tests / simplicity / security`）を適用します。findings があれば **fix turn** が潰して commit、プロジェクトの check を再実行し、review に戻ります。収束は forge マーカーではなく **ローカルのラウンドカウンタ**（`review.max_rounds`）で縛り、上限に達しても clean にならなければ block せず PR を公開します（pr-reviewer / 人間の merge ゲートが最後の砦）。会話には一切投稿しません — review turn は routing の `self-review` profile で走り（author とは別モデルにもできます）、結果は会話タイムライン外に記録されます: push 後 head の `meguri/self-review` commit status と、PR 本文の折り畳み `<details>`。`review.enabled = false` で丸ごと止められます（外部 bot がレビューを担う場合など）。

**GitHub pr-reviewer** は任意の外部レビューループ（`runs.loop_kind = "pr-reviewer"`）で、project × kind で切り替えます（`review.guard.plan` — 既定 ON、旧 spec reviewer / `review.guard.impl` — 既定 OFF）。開いた PR を独立した `pr-reviewer` profile でレビューし、同じ形式で verdict を残します — `meguri/pr-review` commit status + PR 本文の折り畳み `<details>` — **inline スレッドは決して作りません**。したがって **fixer** は反応せず、AI↔AI の ping-pong は畳まれたままです。plan のレビューは spec ラベルも駆動します（clean → `spec-ready`）。人間にとって赤い pr-review チェックは **advisory** です（`meguri/pr-review` を required check に指定しない限りマージは止めません）。auto-merge にとっては **gate** です（後述）。

AI が thread を作らないので、**fixer** の discover は自然と人間・外部 bot の thread だけを拾います — GitHub をレビュー transport に使うのは「人間が居る側」に限定されます。

### cleaner（read-only のリポジトリ巡回）

**cleaner** ループは default branch の head を定期的に歩いて回り、蓄積した乖離 — spec と実装のずれ、dead code の候補、規約からの逸脱、置き去りの TODO、stale なリモートブランチ、孤児化した `meguri:working` ラベル — を `meguri:clean-report` ラベル付きの **1 本のレポート issue**（1 project = 1 issue）に書き留めます。修正は一切しません: 書き込みはこの issue の作成・更新だけで、push もブランチ操作も、他の issue / PR へのラベルやコメントもしません。本文は巡回のたびに完全に書き直されるスナップショットで、隠しマーカーの head sha により同じ head が二度走査されることはなく、head が進んでも `clean.interval_hours` を過ぎるまで次の巡回は走りません。検出項目を採用するなら通常の issue を切って `meguri:plan` / `meguri:ready` を付け、誤検知なら `clean.ignore` に部分文字列を足し、ループを止めたければレポート issue に `meguri:hold` を貼ってください。

### triage（read-only の推薦巡回）

discovery のトリガーは今も、人間が issue に `meguri:ready` / `meguri:plan` を貼ることに頼っています。**triage** ループは、この最後の手ラベル付けを自動化する第一歩です — そして cleaner と同じく、まず read-only から始めます（ADR 0006）。*未トリアージ*の open issue（open・`meguri:` ラベルなし・hold でない・未解決 blocker なし）を 1 件ずつ見て、「meguri がどう扱うべきか（`ready` / `plan` / `needs-human` / `hold` / `skip`）・確信度・おおよその規模」の推薦を、`meguri:triage-report` ラベル付きの **1 本のレポート issue** に書き出します。v0 は **推薦するだけ**です: トリアージ対象の issue にラベルもコメントも一切付けないので、判断を誤っても壊れるものがありません。推薦を採用するときは、あなた自身が `meguri:ready` / `meguri:plan` を貼ってください — あとは既存のループが引き継ぎます。本文は巡回のたびに書き直されるスナップショットの表で、`triage.interval_hours` で律速されます。default branch の head が止まっていても、新しい issue が立てば巡回が走ります（新規 issue が次の push を待たずにトリアージされる)。誤った推薦は `triage.ignore` に部分文字列を足して黙らせ、ループを止めたければレポート issue に `meguri:hold` を貼ってください。triage は **オプトイン**です: `triage.mode = "report"` にするまで何もしません（既定は `off` — 観測ではなく判断を自動化するため)。ラベルの自動付与（`advise`、そして `auto`）は将来の課題です。

### reconcile（issue 本文の編集は再着手シグナル）

一度 succeeded した run が処理した issue は、以後 meguri が再ディスカバリしなくなります（さもないと毎ポーリングで同じ仕事を再ファイルしてしまう)。ただし従来この抑止は**恒久的**で、あとから概要欄を編集しても何も起きませんでした。**reconcile** ループはこの抑止を**本文アウェア**にします（本文を空白正規化したダイジェストで比較するので、GitHub の `updatedAt` を動かすだけのラベル付け替えは無視され、空白だけの編集もカウントされません）。本文が実質的に編集されると抑止が解け、durable な `issue.body_changed` イベントが記録され（`meguri logs` から追える)、poll sweep が `implementing` の issue に「再実装するなら `meguri:ready` を付け直して」というコメントを 1 度だけ残します。

**本文編集はトリガーではなくシグナルです。** それ単体で agent を起動することは決してありません — 起動ゲートは collaborator が付けるフェーズラベルのまま（プロンプトインジェクションを縛る[ラベルゲート](#ラベル)と同じ: 「誰が agent を起動できるか」=「誰が write 権限を持つか」)。本文編集は issue を再び**対象候補**にするだけで、実際に走らせるには collaborator が `meguri:ready` を（付け直）す必要があります。シグナルもコメントも新しい本文ごとに高々 1 回なので、未処理の編集がログを溢れさせることはありません。ループごと止めるなら `reconcile.body_edits = false`、検知は残してコメントだけ消すなら `reconcile.signal_comment = false`。

GitHub 上のラベルとコメントが永続的なワークフロー状態です（looper の「Authority」原則）。ローカルの sqlite（`~/.meguri/meguri.sqlite`）は実行（run）の進行のみを追跡します。meguri はいつ kill しても構いません — `meguri watch` が復旧します: 生きている pane は再アダプトされ、死んだ run は最後にチェックポイントされたステップから再開されます。pane・claude session・worktree は **issue が寿命の単位**です — branch を編集するループ全員が共有する **author** pane が 1 枚（planner → spec fixer → worker/spec worker → fixer/ci fixer/conflict resolver が同じ live session で文脈を継ぐ）と、pr-reviewer 専用の独立した **pr-review** pane が 1 枚（さらに run が self-review する間だけ一時的な **self-review** pane）。turn が完了するたびにエージェントのネイティブ session id が issue の lane に保存されるので、idle 中に pane が死んでも次の run は同じ会話に `claude --resume <id>` で復帰します。watch 中は issue が close されると対応する pane・worktree・マージ済みローカルブランチが自動回収されます。一発実行運用では `meguri prune` で同じ掃除ができます。

ループ別の寿命の一覧:

| loop | trigger | 鍵 | worktree | 正常終了 | pane 後始末 |
|---|---|---|---|---|---|
| planner (author) | `meguri:plan` issue | issue | 新 branch | spec PR 作成 → `spec-reviewing` | keep |
| pr-reviewer (pr-review) | レビュー対象 PR（spec/impl）/ head 未レビュー | issue + `pr-review` | read-only detached（`pr-reviewer-<issue>` 固定） | `meguri/pr-review` status + 本文 `<details>`; plan clean → `spec-ready` | keep（独立） |
| spec fixer (author) | `spec-reviewing` PR の head の plan レビューが赤 | issue（branch 復元） | PR head に attach | spec 修正 push（≤3 round） | keep・author pane を継ぐ |
| spec worker (author) | `spec-ready` PR（combined のみ） | issue（branch 復元） | 既存 branch を継ぐ | 実装 → PR 更新 | keep・author pane を継ぐ |
| worker (author) | `meguri:ready` issue | issue | 新 branch | self-review → PR `Closes #N` | keep |
| fixer (author) | PR の未解決スレッド | issue（branch 復元） | PR head に attach | スレッドに再 review 依頼返信 | keep・author pane を継ぐ |
| ci fixer (author) | meguri PR の CI 赤 | issue（branch 復元） | PR head に attach | fix push（≤3 round） | keep・author pane を継ぐ |
| conflict resolver (author) | PR が Conflicting（≤3） | issue（branch 復元） | PR head に attach | base merge & 解消 → push | keep・author pane を継ぐ |
| cleaner (standalone) | レポート issue + 既定 branch 前進 | レポート issue | read-only detached | 単一レポート issue 再生成 | 自前回収 |
| triage (standalone) | レポート issue + 既定 branch 前進 / 新規 issue（オプトイン） | レポート issue | read-only detached | 単一レポート issue 再生成 | 自前回収 |

### 自動マージ（オプトイン）

meguri は「マージして安全か」を自前で判定しません — 条件の揃った PR に GitHub ネイティブの auto-merge を arm する（`gh pr merge --auto`）だけで、いつマージするかの最終判断は GitHub（branch protection + required checks）に委ねます（`docs/adr/0003-auto-merge-github-native-arm-only.md` 参照）。デフォルトは無効で、二段のオプトインでゲートします: マスタースイッチ `[pr.auto_merge].enabled` と、（`opt_in = "all"` でない限り）`meguri:automerge` ラベルです。ラベルを *issue* に貼ると worker が PR へコピーします（その PR は最初から non-draft で開きます）。PR に直接貼っても効きます。

watch のポーリングに相乗りする sweep が、**すべて**満たした PR を arm します: `meguri/` ブランチで `Closes #N.` により issue に紐づいている / `meguri:hold`・`meguri:needs-human`・`meguri:working`・`meguri:spec-reviewing`・`meguri:spec-ready` のいずれも付いていない（spec フェーズ中は絶対に arm しない）/ 未解決 review thread がゼロ / リポジトリが auto-merge と設定した strategy を許可している（必要なら required checks 付き branch protection もある）。**impl pr-reviewer** が有効なら gate になります（ADR 0008）: `meguri/pr-review` が success の head だけ arm し、failure は `meguri:needs-human` へエスカレーション、未到達/pending は待機します（pr-reviewer 無効なら要求する status が無いのでデッドロックしません）。arm はレビュー済み head に `--match-head-commit` で固定され、マーカーコメント（`<!-- meguri:automerge armed head=<sha> -->`）が冪等性と人間の上書き尊重を担います — 人間が後で auto-merge を解除した head は再 arm しません（新しい push で再判定）。arm しようとした時点で GitHub が既に「マージ可能」と判定していた場合は、meguri がその判定に従ってマージを確定します。

```toml
[pr.auto_merge]
enabled = false                  # マスタースイッチ
mode = "native"                  # native(GitHub auto-merge を arm)| orchestrator(meguri 自身がマージ)
strategy = "squash"              # squash | merge | rebase(リポジトリで不許可なら fallback せず拒否)
require_branch_protection = true # required checks 付き protection がなければ arm しない
opt_in = "label"                 # label(meguri:automerge が必要) | all(全 meguri PR が対象)
```

`enabled = true` なのにリポジトリが auto-merge を honor できない（auto-merge 不許可・strategy 不許可・protection なし）場合、`meguri watch` 起動時と `meguri doctor` で **fail-fast** します（マージ時に静かに劣化させない）。逃げ道は同じ `require_branch_protection = false` で、注意点が二つ: protection 検出は **classic branch protection API のみ**（rulesets は検出できない）で、その参照には **admin 権限のトークン**が必要です（admin でないトークンは HTTP 403 になり、meguri はそれを「protection なし」に倒さずエラーとして返します）。また meguri 自身のレビューをマージ前提にしたい場合は **impl pr-reviewer**（`review.guard.impl = true`）を有効化してください: auto-merge は `meguri/pr-review` が success の PR だけ arm します（ADR 0008）。impl pr-reviewer が OFF ならこの gate は無いので、オプトイン PR は meguri の外部レビュー前でも required checks さえ通れば merge され得ます — 求める品質バーは branch protection（と必須の内部 self-review）で担保してください。

**`mode` — native と orchestrator。** 既定の `native` は上記のとおり（meguri は arm するだけ、判断は GitHub）。しかし **private + Free プランのリポジトリでは "Allow auto-merge" 自体が有効化できず**（API PATCH が黙って無視される）branch protection も無いため、`native` は必ず fail-fast します — meguri 自身が `docs/adr/0004-automerge-gate-renovate-side-on-free-private.md` で直面したのと同じ制約です。`mode = "orchestrator"` はまさにその環境向けのフォールバックで、適格判定（ブランチ / リンク / ラベル / スレッド）は native と同一のまま、arm する代わりに **GitHub が `MERGEABLE` を返した時点で meguri 自身が直接マージ**します（`gh pr merge --squash` 相当、レビュー済み head に固定）。`CONFLICTING` は conflict-resolver に委ね、`UNKNOWN` は次の sweep に持ち越します。サーバ側ゲートが無いため、orchestrator モードは **meguri 自身の PR 前検証（`check_command` + self-review）を唯一のゲートとして明示的に受容**します（`docs/adr/0009-auto-merge-orchestrator-side-merge-on-free-private.md`）。`meguri doctor` もその旨を注意表示します。orchestrator モードは `require_branch_protection = false` が必須です（矛盾する組み合わせは config 検証で弾かれます）。"Allow auto-merge" を有効化**できる**環境では `native` のままにしてください — サーバ側ゲートは常にプロセス内ゲートより強いためです。

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

[triage]
mode = "off"            # off（既定・オプトイン）| report（v0 read-only）| advise（v1）| auto（v2）
interval_hours = 6      # triage の巡回間隔の下限
ignore = []             # 誤った推薦を黙らせる部分文字列。プロジェクト単位は [projects.triage] で上書き

[review]
enabled = true    # 内部 self-review フェーズ（plan + impl）のキルスイッチ
max_rounds = 3    # run ごとの self-review ラウンド上限。超えたら PR をそのまま公開する
lenses = ["correctness", "tests", "simplicity", "security"]  # 多角視点レンズ（ADR 0008）
# （旧 impl_enabled / impl_max_rounds キーも alias として読み込めます）

[review.guard]    # 任意の外部 pr-reviewer レビュー（kind 別、ADR 0008）
plan = true       # spec/ADR PR をレビュー（旧・必須 spec reviewer）— 既定 ON
impl = false      # 実装 PR をレビュー — 既定 OFF（opt-in・外部 bot 互換）

[reconcile]
body_edits = true      # 処理済み issue の本文編集を検知して再着手シグナルとして扱う
signal_comment = true  # 「meguri:ready を付け直して」の促しコメントも残す（false なら durable なイベントのみ）
```

plan 経由の納品形は project ごとに `plan_delivery` で選びます（既定 `separate` = 2 本の PR / `combined` = #98 の 1 本 morph 型）。`[pr]`・`[clean]` と同じく `[projects.review]` は `[review]` セクションを丸ごと上書きします。

`[projects.pr]` は `[pr]` セクションを（キー単位ではなく)丸ごと上書きします: `[projects.pr]` を書いたプロジェクトは、省略したキーはデフォルトになり、`[pr.auto_merge]` も含めてそうなります。

### workspace — 関連プロジェクトと cross-repo 分解（オプトイン）

**workspace** は関連プロジェクトの静的なグルーピングです（repo の分割/統合、API とそのクライアント、repo 設計込みの greenfield など）。純粋に宣言的で、**実行系(worktree・pane・branch・検証)には一切現れません**（`run` は単一 repo のまま）。状態も持ちません。オプトイン: `[[workspaces]]` を書かない config の挙動は従来と完全に同じです。

```toml
[[workspaces]]
id = "shop"
projects = ["shop-api", "shop-web", "shop-infra"]   # 各要素は定義済みの [[projects]] id。1 プロジェクトが所属できる workspace は 1 つまで
```

workspace の用途はちょうど 3 つです:

1. **分解のスコープ** — planner の decompose([spec 先行フロー](#spec-先行フローオプトイン))は、子 issue に `"project": "<sibling id>"` を付けることで workspace 内の別 repo に起票できます(省略時は親と同じ repo)。親(tracking)issue は常に自分の repo に留まります。workspace の外の repo を指す子は拒否されます — 起票スコープは(write 権限で操作できる)issue body ではなく config 側(ホスト運用者)に置くことで、「実行させられる人」と「スコープを決める人」を分離します(ADR 0009)。
2. **cross-repo の順序付け** — meguri は GitHub ネイティブの `blocked_by` を repo をまたいで張ります。片方の repo の子がもう片方の repo の子をブロックでき、既存の discovery の依存ゲートがそれらを順序づけます(読めない blocker は未解決＝安全側で止まる)。
3. **表示のグルーピング** — `meguri ps` / `meguri top` が行を workspace 単位で束ねます。

meguri 自身が実行できない操作(repo 作成、公開設定の変更、履歴書き換えなど)には、`"kind": "human"` の子を使います。これは**トリガーラベルなし**で起票され、discovery は決して拾わず、人間が閉じることで依存側が解放されます。`meguri doctor` は各 workspace とメンバーを一覧します。設計の意図は ADR 0009 を参照してください。

### worktree セットアップフック（オプトイン）

`[projects.worktree_setup]` は、meguri が worktree を準備するたびに(初回だけでなく create/attach/re-point のたびに)プロジェクト独自のコマンドを実行します。`attach_worktree`/`create_review_worktree` は再利用時に `reset --hard` + `clean -fd` で untracked なファイルを消すことがあるため、毎回の実行が必要になります。meguri 自身はここで何を実行するかに関与しません(ADR 0003)。apm(「[エージェント向け指示（apm）](#エージェント向け指示apm)」参照)はその一利用例であり、専用の組み込み連携ではありません:

```toml
[projects.worktree_setup]
commands = ["apm install --frozen"]        # sh -c で順に実行。途中で失敗したら以降は実行しない
exclude = [".claude/rules", "AGENTS.md"]   # .git/info/exclude に追記(常時追記される .meguri/ に加えて)
required = false                           # true にすると失敗時に run が失敗扱いになる(既定は warn して続行)
timeout_secs = 300                         # コマンドごとのタイムアウト。ネットワーク fetch を伴い得るため
```

コマンドは worktree を `cwd` として実行され、`MEGURI_ROLE`(run のロール — `worker` / `fixer` / `spec-reviewer` など)、`MEGURI_PROFILE`(解決された起動プロファイル)、`MEGURI_ISSUE`(対象の issue/task 番号)が環境変数として渡されるので、ロールごとにスクリプト側で最適化できます。コマンドは同じ worktree に対して複数回実行され得るため、冪等に書いてください。

meguri 自身のループにこのフックを配線した実例(#139、dogfood 検証込み)は [docs/ops/apm-worktree-setup.md](docs/ops/apm-worktree-setup.md) を参照してください。`apm install --frozen` は毎回 `apm.lock.yaml`(git 追跡ファイル)の `local_deployed_files` を書き換えるため、`commands` に `git checkout -- apm.lock.yaml` を続けて入れないと、エージェント自身が触っていないファイルのせいで clean-tree 検証が落ちます — `exclude` は未追跡ファイルにしか効かないので救えません。

### 時刻駆動の起票(`[[projects.schedules]]`、オプトイン)

日次の生産タスクや週次の棚卸しといった定期運用のために、プロジェクトは cron **スケジュール**を持てます。スケジュールがするのは**キューに1件積む**ことだけです(github mode ならラベル付き issue、local mode ならローカルタスク)— 積まれた仕事は、あなたが手で起票したのとまったく同じように既存の worker/planner ループが消化します。meguri は時刻起動で任意コマンドを実行することは**しません** — 起票が仕事のすべてで、実行はループの仕事です(ADR 0009)。これは `meguri add` の「定期版」にあたり、`meguri watch` がポーリングのたびに評価します。

```toml
[[projects.schedules]]
name = "daily-tidy"              # プロジェクト内で一意
cron = "0 9 * * *"              # 標準5フィールド cron、UTC 解釈
kind = "ready"                  # "ready" → worker(meguri:ready)| "plan" → planner(meguri:plan、github のみ)
title = "Daily tidy {{date}}"  # テンプレート、変数は {{date}}(発火日付、YYYY-MM-DD UTC)のみ
body_file = "ops/daily-tidy.md" # repo 相対の本文ファイル — または `body = "..."` インライン(どちらか一方)
# allow_overlap = false         # 既定: このスケジュール由来の直近 issue/task が open な間は起票をスキップ
```

- **cron は UTC** 解釈で、ポーリング間隔の粒度で評価します(5フィールド: 分・時・日・月・曜日。`*`・範囲・`*/n` ステップ・リストに対応)。ローカル時刻で回したいなら式をずらしてください。per-schedule のタイムゾーン指定は将来追加します。
- **catch-up は折りたたみます。** 最終発火時刻は sqlite に永続化されます(config ではないので、定義を hot reload で書き換えても失われません)。`watch` が複数の発火時刻をまたいで停止していても、次の tick で**1回だけ**発火します(cron デーモンの一般則)。新規追加したスケジュールは過去分を backfill せず、最初の tick は「観測した」記録だけを残します。
- **重複ガード。** 既定では、直近の issue/task が open な間はスキップします(そのときの発火時刻は消費するので、後で close されても遡って発火しません)。遅い仕事が重複を溜めないためです。`allow_overlap = true` で毎回発火します。
- **来歴。** 発火した各アイテムは本文に hidden マーカー `<!-- meguri:schedule name=<name> -->` を持ちます(ローカルタスクは加えて `origin = schedule:<name>`)。
- 定義は hot reload(#73)対象です。スケジュールを足す/変えると次 tick から効き、`watch` の再起動は不要です。`meguri doctor` は cron 式・名前の一意性・本文の排他・`body_file` の実在を検証し、`meguri schedules` は各定義を最終発火・次回発火とともに一覧します。

local mode には planner が無いため、`kind = "plan"` は github 専用です — local の `plan` スケジュールは config ロード時に拒否されます(タスクが消化されないため)。

### discovery の調速: not-before と cadence(`[[projects.cadence]]`、オプトイン)

時刻駆動運用の片輪が起票なら、もう片輪は**消化のペース制御**です。discovery は通常、並列上限の許す限りキューを消化するので、時刻に縛られた2種類の仕事にはブレーキが要ります(issue #148)。どちらも**サイレントにスキップ**します — ラベルもコメントも forge に残しません(ブロックされた GitHub-native 依存とまったく同じ流儀)— そして `meguri tasks` で見えます。

- **not-before** — 「この日時までは着手しない」。github mode は issue 本文の hidden マーカー、local mode は `--not-before` で指定します:

  ```
  <!-- meguri:not-before 2026-07-20 -->          # 裸の日付は UTC 深夜
  <!-- meguri:not-before 2026-07-20T09:00:00Z --># または完全な RFC3339 UTC
  ```
  ```sh
  meguri add --not-before 2026-07-20 "公開ポスト"
  ```
  日付のタイポは fail-**closed**(タスクは止まったままで `meguri tasks` に表示)。早期公開の事故を避けます。

- **cadence** — 「このラベルは窓あたり N 件まで消化」。ラベルごとのレート上限を宣言すると、discovery は消化実績をローカルの run 履歴から数え(GitHub からは数えません — ラベルは workflow 状態、実行の記録はローカル)、窓が埋まっている間そのラベルを止めます:

  ```toml
  [[projects.cadence]]
  label = "sns"          # github の issue ラベル
  max_per_day = 1        # UTC 暦日あたり最大1件
  # — 暦日ではなくローリング窓にするなら: —
  # per_hours = 168
  # max = 1
  ```
  cadence は github 専用です(local タスクにラベルが無いため)。消化は benign な skip を除く全試行を数えます — 失敗した run も当日の枠を消費するので、壊れた投稿が媒体のレート上限を超えてリトライすることはありません。2つの rule に一致する issue は fail-closed(run はひとつのバケツにしか計上できないため)。`meguri run --issue N` はゲートをバイパスしますが窓には計上します。`meguri doctor` が各 rule の現在の窓消化を表示します。

### ロール別 preamble(`[prompts]`、オプトイン)

「作業前にこのガードレールを読む」「この編集ペルソナに従う」「この品質基準を満たさないものはコミットしない」といった、issue 個別ではなくプロジェクト全体に常時かかる規律を、ロール別に turn プロンプトへ埋め込みます。worker には品質基準、planner には企画ガイドライン、reviewer には監査観点、と出し分けられます。値は **repo 相対パス**で、その中身がプロンプト冒頭(前文 — 完了契約は末尾のままで優先されます)に埋め込まれます。

```toml
[prompts]                          # top-level 既定(全プロジェクトに適用)
all = "ops/agents/guardrails.md"   # 全ロール共通
worker = "ops/agents/worker.md"    # キーは routing の6ロール(worker/planner/fixer/self-reviewer/pr-reviewer/cleaner)

[projects.prompts]                 # per-project override(キー単位で上書き)
planner = "ops/agents/planner.md"
```

- **参照ではなく埋め込み** — profile が Claude でも Codex でも、agent がファイルを開こうと開くまいと、規律が届きます(この CLI 非依存が存在理由の半分。[ADR 0012](docs/adr/0012-role-preamble-injected-into-turn-prompt.md))。
- **`all` → ロール別**の順で両方注入。per-project エントリはキー単位で top-level を上書きします(語彙・別名は `[routing.roles]` と同一。未知のロールキーは config ロードで拒否)。
- **欠落は致命ではない** — 存在しないパスや worktree 外へ抜ける symlink は warn + `prompt.preamble_missing` イベントで飛ばし、turn は続行します。`meguri doctor` は clone 内に解決しないパスを報告します。
- **`CLAUDE.md` との使い分け**: 全ロール同一の常時コンテキストで足り、Claude だけで回すなら [エージェント向け指示(apm)](#エージェント向け指示apm) / `CLAUDE.md` で十分です。ロール別テキストや CLI 非依存の配達が要るときだけ `[prompts]` を使い、ファイルは短く保ってください(かさばる内容は `CLAUDE.md` の担当)。

### collab アドバイザ(`[collab]`、オプトイン)

routing は「どの役割をどのモデルに振るか」を決めました。collab アドバイザはその**次段**です。振り分けた役割モデル同士を、**実行中に喋らせます**。worker が実装している間、meguri は plan を書いた役割(`planner`)を**アドバイザ**として同じ issue に再具現し、worker は [agmsg](https://github.com/fujibee/agmsg) 越しに「この方針で spec の要件を満たすか/ブレていないか」を相談できます。ドリフトを PR より前に、安く摘む層です(内部 self-review・pr-reviewer とは畳まず補完。[ADR 0006](docs/adr/0006-collab-advisor-role-reembodiment.md))。

```toml
[collab]
mode = "advisor"          # "off"(既定)| "advisor"
advisor_role = "planner"  # アドバイザが借りる routing ロール
```

- **`[collab]` がスイッチ、`mode = "advisor"` が ON。** セクション無し(または `mode = "off"`)は現状とバイト単位で同一です — アドバイザは立たず、worker のプロンプトも不変。`mode = "advisor"` のときだけ有効になります。
- **起動時に大きな音で落ちる。** `mode = "advisor"` で agmsg skill(`~/.agents/skills/agmsg/scripts/version.sh`)が見つからなければ、`meguri watch` / `meguri run` は起動時に明示エラーで止まります(routing と同じ流儀、silent fallback しない)。`[collab]` は process-bound で、編集は再起動で反映されます(稼働中の hot reload は warn して起動時の値を維持)。
- **アドバイザは ephemeral。** worker の実装開始で spawn、run 終了で reap(`keep_pane` に依存しない)、resume・再起動では adopt せず捨てて張り直し、書き込み可能な checkout を持ちません — 助言するだけでコードは書きません。プロファイルは plan 作者が実際に使ったものを継ぎます。collab 有効の worker run はスケジューラ枠を 2 消費します。
- **相談は助言であって完了条件ではありません。** meguri は agmsg のやり取りを読まない・待たない・検証しません — run の成否は従来どおり `result.json` + git 検証だけで決まります。プロトコルは両エージェントのプロンプトに置かれます。v1 の対象は `worker` / `spec-worker` です。

## 開発

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

テストスイートは、スクリプト化された偽エージェント TUI（`tests/fixtures/fake_agent.sh`）を使い、本物の tmux・本物の git worktree・ローカルの bare origin に対してループ全体を駆動します — blocked ダイアログの処理、嘘をつくエージェントの矯正、検証フィードバック、クラッシュリカバリを含みます。

loop がどう繋がっているか（パイプライン全体図・ディスパッチ優先度・loop 別ライフサイクル・ADR 索引）を設計者向けにまとめた地図は [docs/architecture/loops.md](docs/architecture/loops.md) を参照してください。この README は引き続き利用者向けの「使い方」、あちらは設計者向けの「なぜこの構造か」です。

### エージェント向け指示（apm）

meguri 自身のリポジトリ固有の AI エージェント（Claude Code / Codex）向け指示は、手書きの `CLAUDE.md` / `AGENTS.md` ではなく [microsoft/apm](https://github.com/microsoft/apm)（`apm.yml`・`apm.lock.yaml`・`.apm/instructions/`）をソースにしています。コンパイル成果物（`CLAUDE.md` / `AGENTS.md` / `.claude/rules/` / `.codex/` / `apm_modules/` / `.agents/`）は `.gitignore` に入れてあります — 指示を1行直すたびに並行中の worktree/PR 全部で再生成 diff が出るのを避けるためです（[ADR 0008](docs/adr/0008-agent-instructions-via-apm.md) 参照）。ローカルで生成するには:

```bash
brew install microsoft/apm/apm   # または: curl -sSL https://aka.ms/apm-unix | sh
apm install                      # .apm/instructions/ を .claude/rules/ に展開
apm compile                      # Codex 向けに AGENTS.md（+ src/AGENTS.md）を生成
```

順序が重要です: `apm compile` が `CLAUDE.md` を生成しないのは、直前の `apm install` が `.claude/rules/` を先に展開しているからです(Claude Code はそちらを直接読むので、apm が重複コンテキストとして `CLAUDE.md` を除外する)。先に `apm compile` を実行した場合や、空のツリーに対して実行した場合(例: 隔離検証用の `--root <scratch-dir>`)は、除外対象がまだ無いため `CLAUDE.md`/`src/CLAUDE.md` も生成されます。`apm install --dry-run` もこのステップのプレビューにはなりません — dry-run が報告するのは `apm`/`mcp` パッケージ依存(このリポジトリには無い)だけで、ローカルの `.apm/instructions/` 展開は対象外です。`.claude/rules/` を実際に展開するには dry-run なしの `apm install` が必要です。

`.apm/instructions/` や `apm.yml` を編集したら両方を再実行してください。実際に `apm install` を実行すると `apm.lock.yaml` の `local_deployed_files` / `local_deployed_file_hashes` もディスク上の現在のデプロイ状態に合わせて書き換わります — これらは gitignore 対象のコンパイル成果物を追跡しているだけなので、その差分はコミットせず、コミット前に `git checkout apm.lock.yaml` で戻してください(`apm lock` を再実行しても、これらのフィールドは既存の lockfile から引き継がれて消えません)。meguri にはこのビルドを worktree 準備のたびに自動実行できる汎用の [worktree セットアップフック](#worktree-セットアップフックオプトイン)(`[projects.worktree_setup]`)がすでにあり、meguri 自身のループにも配線済みです(#139、手順と実機検証は [docs/ops/apm-worktree-setup.md](docs/ops/apm-worktree-setup.md))。

## ステータス / ロードマップ

GitHub 上で 10 のループが動きます。looper のロールモデルを踏襲し、いずれも同じターンエンジンを共有する `Loop` 実装です: **worker**（issue → self-review → PR）、**planner**（`meguri:plan` issue → self-review → spec PR）、**pr-reviewer**（レビュー対象 PR（spec/impl）→ `meguri/pr-review` commit status + PR 本文の折り畳み `<details>` にサマリレビューを記録。plan のレビューは `spec-reviewing → spec-ready` も張り替える）、**spec fixer**（head の plan レビューが赤で確定した `meguri:spec-reviewing` PR → pr-reviewer の findings を author lane に戻して spec を修正し、同じ PR に push（pr-reviewer が新しい head を再レビュー）。3 回の修正ラウンド後もまだ赤なら `meguri:needs-human` にエスカレーション）、**spec worker**（combined 納品時の `meguri:spec-ready` PR → 同じブランチ・同じ PR に実装コミットを積む）、**fixer**（meguri の PR の未解決レビューコメント → 修正コミットを push）、**ci fixer**（CI チェックが赤で確定した meguri の PR → 失敗ジョブのログを agent に渡す → 修正コミットを push。3 回の修正ラウンド後もまだ赤なら `meguri:needs-human` にエスカレーション）、**conflict resolver**（CONFLICTING な meguri の PR → ベースブランチを取り込み、コンフリクトを解消したマージコミットを push）、**cleaner**（定期的な read-only 巡回 → 乖離レポートを 1 本の `meguri:clean-report` issue に）、**triage**（オプトインの read-only 巡回 → 未トリアージ open issue への推薦を 1 本の `meguri:triage-report` issue に）。必須の内部 **self-review**（ADR 0006/0008）はループではなく worker と planner が PR を開く前に run の worktree で回すフェーズで、軽量な **plan→impl handoff** 掃引が separate 納品の spec を進めます（spec PR マージで `speccing → ready`）。どちらも会話タイムライン外です。

**バージョニング。** meguri は 1.0 前（`0.x`）で [SemVer](https://semver.org/lang/ja/) に従います: `0.x` の間は public API と CLI が未安定で、minor（`0.y`）が破壊的変更を含みうる一方、patch（`0.y.z`）は互換を保ちます。安定を約束するのは `1.0.0` からです。現在の挙動に依存する場合はバージョンを固定してください。

**リリース。** リリースはタグ駆動です（ADR 0007）: メンテナがバージョンを bump し、`CHANGELOG.md` を更新して `vX.Y.Z` タグを push すると、`.github/workflows/release.yml` が macOS arm64 / Linux x86_64 のバイナリをビルドして GitHub Release に添付し（本文は git-cliff 生成のノート）、（crate 設定が済めば）OIDC Trusted Publishing で crates.io に publish します。**push したタグがそのままリリースの起点**なので、タグは慎重に — 誤タグは誤リリースになります。

## コントリビューション

人間からのバグ報告・PR を歓迎します — 通常の fork & PR フローで、`meguri:*`
ラベルを気にする必要はありません。詳細は [CONTRIBUTING.md](CONTRIBUTING.md)
（英語）を参照してください。

## ライセンス

MIT
