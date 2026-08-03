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
- **issue 本文はプロンプト入力です。** issue 本文全体（とループが読むコメント）はそのままエージェントのプロンプトに注入されます。誰でも issue を開けるリポジトリでは、悪意ある issue 本文はシェルアクセスを持つエージェントへの prompt injection の試みになり得ます。その緩和策が「[ラベルによるゲート](#ラベル)」です：ループは `meguri:ready` ラベルが既に付いている issue にしか反応せず、ラベルを付与できるのは collaborator（write 権限）だけです。つまり「誰がエージェントを動かせるか」は「誰がこのリポジトリへの write 権限を持つか」に還元され、「誰が issue を開けるか」には依存しません。collaborator 権限を付与する際はこれを踏まえ、信頼できない issue に自分で `meguri:ready` を付けないでください。
- **完了判定は画面パースではなく独立検証です。** 上記「[完了契約（completion contract）](#完了契約completion-contract)」の通り、meguri はエージェント自身の「成功しました」という主張をそのまま信用しません。run を完了扱いする前に git の状態、base より先行するコミット、プロジェクトの `check_command` を再検証します。これは侵害された/誤誘導されたエージェントの被害範囲を（完全にではありませんが）限定します — run の最中に worktree 内で何かをすることはできても、result ファイルに「成功」と書くだけで meguri に不正な状態をマージさせることはできません。
- **pre-flight prime はフォルダ信頼だけを担い、ツールは動かしません。** `claude` CLI は worktree ごとの初回起動で「このフォルダのファイルを信頼しますか？」と尋ねてきますが、meguri は画面を読まないので答えられません。そこで対話 pane を起動する直前に、その worktree で headless の `claude` を一度だけ走らせ（pre-flight prime）、CLI 自身にそのパスのフォルダ信頼を記録させます。この一回は `--dangerously-skip-permissions` を付けず、meguri 所有の全ツール deny な `--settings` ファイルと `--strict-mcp-config` の下で走るので、**ツールを一切実行しません** — worktree 内の悪意ある `CLAUDE.md` が、継承した緩い設定の下でも、pane 起動前に Bash/Edit/MCP を動かすことはできません。pane と同じ `--model` を使い、書くのはフォルダ信頼だけで、config-dir 単位の「Bypass Permissions」受諾（これは doctor が案内する一度きりの手順）は書きません。worktree ごとに高々一度だけ走り、起動を失敗させることはありません — エラー・timeout・CLI が deny フラグ非対応に古い場合は、pane はこれまで通り起動します。profile で `preflight = []` にすると無効化できます。明示的に非空の `preflight` を書くと安全な既定を上書きでき、それは自己責任です（yolo なものは injection 無防備で、meguri は起動時に警告します）。

meguri 自体の脆弱性を見つけた場合は [SECURITY.md](SECURITY.md) を参照してください。

## インストールとセットアップ

前提: `git`、[`gh`](https://cli.github.com)（認証済み）、エージェント CLI（デフォルトは `claude`）、そしてマルチプレクサ — 起動中の [herdr](https://herdr.dev)（推奨。エージェント状態のネイティブ検出）または `tmux`（画面ヒューリスティックのフォールバック）。これらのランタイム前提はインストール方法によらず同じです — 配布バイナリを使う場合もホストに `git`/`gh`/マルチプレクサが必要です。

対応プラットフォーム: meguri は macOS / Linux で動作します。

```bash
cargo install --path .            # or: cargo build --release
meguri init                       # ~/.meguri/config.toml を作成（プロジェクトは 0 件）、db も作成
meguri doctor                     # gh 認証・mux・agent CLI を検査
```

バイナリの入手方法（その他）:

- **配布バイナリ** — [最新の GitHub Release](https://github.com/kkato1030/meguri/releases/latest) から自分のプラットフォーム（macOS arm64 / Linux x86_64）のアーカイブをダウンロードし、`.sha256` で検証・展開して `meguri` を `PATH` に置きます。
- **crates.io** — `cargo install meguri`（crate の publish 後。[ステータス / ロードマップ](#ステータス--ロードマップ) を参照）。

**プロジェクト追加は `config.toml` を編集。** `meguri init` は **プロジェクト 0 件**の最小 `~/.meguri/config.toml` を書きます（`[[projects]]` スタブはコメントアウト済み）。コメントを外して編集してください — meguri は自分で維持している clone の上で動きます（必須の `repo_path` で指定）:

```toml
[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"  # 必須: meguri が worktree を切る元の clone
repo_slug = "owner/repo"          # mode = "local" 以外では必須
# default_branch = "main"
# check_command = "cargo test"   # 推奨: meguri 自身がこれを実行して検証します
```

それ以外はすべて任意です。既定値を上書きしたいセクション/キーだけを書きます（[設定](#設定) を参照）。

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
meguri logs <run>         # event trail + live pane tail
meguri attach <issue>     # issue の agent pane に入る（run id も可）
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
```

### 投入口（`meguri add`）

最初に詰まるのは作業アイテムを起票するところです。`meguri add "<一言>"` はそれを 1 コマンドに下げ、プロジェクトの mode に応じて正しいことをします。

**github mode** — issue は即座に作られます。`create_issue` 直行で、番号と URL を表示します。既定はラベル無し = キュー外（watch は無視）です。あとで `meguri:ready` を付けるか、`--ready` を渡してください — `--ready` はローカルキューへの task 取り込みまで即時に行います（次の intake を待ちません）。

**local mode** — 代わりに meguri の sqlite へ task を積みます（下記参照）。`--file` は markdown ファイルから読みます。

`--project` は cwd から推定されます（`repo_path` が cwd を含むプロジェクト）。曖昧なときは明示してください。

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

ローカル run は `meguri/t<task-id>-<slug>-<hash>` ブランチで作業し、成功すると検証済みコミットをそこに残して task を `done` にします（push はしません）。失敗した run は task を reason 付きの `needs_human` にし（`meguri tasks` / `meguri ps` で見えます）、次の run が再 claim して解除します。ブランチは自分で確認してマージしてください（`meguri review` / `accept` は後のフェーズで入ります）。

> **単一マシン前提。** ローカル sqlite が*唯一の真実*なので、1 リポジトリにつき meguri ホストは 1 台で回してください（watch lock がホスト内の多重起動を防ぎます）。将来のマルチホスト `TaskSource` の語彙と契約は [ADR 0003](docs/adr/0003-tasksource-task-moves-run-pins.md) で固定済みです。

### 常駐させる

`meguri watch` はフォアグラウンドに留まります。シェルを閉じても回し続けたい場合は、tmux/herdr のペイン・`nohup`・自前の launchd/systemd ユニットなど、好みの supervisor の下で動かしてください。どの方法でも watch プロセスは排他ロック（`~/.meguri/daemon/watch.lock`）を握るので、2 つ目のスケジューラは黙って二重駆動せず、明示的に失敗します。

### ラベル

権威反転以降、**meguri のキューの権威はローカル sqlite の `tasks` テーブルであり、GitHub ではありません**。ラベルは低頻度のエッジ入力として読まれ（`scheduler.intake_interval_secs` 周期の intake、既定 120 秒）、best-effort の投影として書き戻されます — ラベル書き込みの失敗が run を止めることはなく、キューの判断が毎 tick の GitHub 読み取りに依存することもありません。

入力（人間が付け、intake が読む）:

| ラベル | 色 | 意味 |
|---|---|---|
| `meguri:ready` | 🔵 青 | この issue を worker ループのキューに入れる（次の intake で task 行として取り込まれる） |
| `meguri:hold` | ⚪ 灰 | 緊急停止（スマホから操作可能）: hold 中の task はディスパッチされない（実行中の run は止まらない — それは `meguri stop` で）。ラベルを外せば解除 |

投影（meguri が書く、best-effort）:

| ラベル | 色 | 意味 |
|---|---|---|
| `meguri:working` | 🟡 黄 | いままさにエージェントが作業中（claim） |
| `meguri:implementing` | 🟢 緑 | 実装 PR が open |
| `meguri:needs-human` | 🔴 赤 | 人間の確認が必要。理由はコメントに。`meguri:ready` が付いたままこのラベルを外すと、次の intake で再キューされる |

🔴 `meguri:needs-human` でフィルタすれば人間の TODO リストになります。新規の meguri ラベルはスキームの色付きで自動作成されます。旧来の汎用青ラベルは一度だけ `gh label edit <name> --color <hex>` で塗り替えてください — meguri は意図して設定した色を上書きしません。

discovery は GitHub ネイティブの issue 依存関係も尊重します: 他 issue に *blocked by* されている issue は、すべてのブロッカーが **completed** で閉じるまで — ラベルもコメントも無しに黙って — スキップされます。*not planned* / *duplicate* で閉じたブロッカーは解決と見なされず（依存側は人間の再トリアージ待ち）、読めないブロッカーは未解決として扱います。

一度 succeeded run で出荷された issue は、ready ラベルが残っていても再取り込みされません（重複 PR ガード）。task が正常に完了したあとで `meguri:ready` を付け直せば、新しい run がキューされます。

meguri はいつ kill しても構いません — `meguri watch` が回復します: 生きている pane は再アダプトされ、死んだ run は最後にチェックポイントした step から再開します。pane・セッション・worktree は issue 単位で、各ターン完了時にエージェントのネイティブセッション id を保存するので、pane がアイドル中に死んでも次の run は同じ会話を再開します（`claude --resume <id>`）。watch 中は、閉じた issue の pane・worktree・マージ済みローカルブランチを自動回収します。`meguri prune` は同じことをオンデマンドで行います。

## 設定

すべての項目に既定値があるため、`config.toml` には `[[projects]]` と上書きしたい項目だけを書けば残りは既定値で埋まります — `meguri init` はその前提の最小テンプレートを書き出します。

`meguri watch` はポーリングのたびに `config.toml` を読み直すので、編集はそれ以降に spawn される run から反映されます — 再起動は不要です（実行中の run は開始時点の設定を保持します）。不正な編集（TOML の構文エラー、projects が空）はログに警告を出して拒否され、直前の有効な設定で動き続けます。プロセスの寿命に紐づく `mux.kind` / `mux.session` だけは例外で、`meguri watch` の再起動が必要です（ログでもその旨を警告します）。

既定値の一覧:

```toml
# エージェントが書く成果物（PR 説明・summary）の言語。自由記述
# （例: "日本語", "English"）。省略するとエージェント任せ（通常は英語）。
# プロジェクト単位は [[projects]] 内の language で上書き。
language = "日本語"

[mux]
kind = "auto"          # auto | herdr | tmux
session = "meguri"     # ベースラベル。プロジェクトごとに専用 workspace
                       # `meguri:<project>`（herdr）/ `meguri-<project>`（tmux）を
                       # 使い、issue タブが混ざらないようにします。
# pane は issue 単位で保持され、issue が閉じると回収されます（先にエージェントの
# ネイティブセッション id を保存: claude --resume <id>）。"never" は run 終了と
# 同時に pane を kill します（高スループット運用）。
keep_pane = "until-issue-closed"  # ほかに: never

[agent]
command = "claude"
# 既定は yolo: エージェントは隔離された worktree の中で走り、git/cargo のたびに
# 許可を求めると自律ループが止まるためです。コマンド単位でゲートしたい場合は
# args = ["--permission-mode", "acceptEdits"] にして pane に attach してダイアログに
# 答えてください。
args = ["--dangerously-skip-permissions"]

[limits]
idle_grace_secs = 90        # 沈黙がこの秒数続くと nudge
nudge_limit = 2             # 人間を呼ぶまでの nudge 回数
max_turn_runtime_secs = 2700
result_grace_secs = 60      # result 出現後 Working→Idle を待つ秒数
validate_turns = 3          # check_command 失敗時の修正ターン上限

[scheduler]
poll_interval_secs = 60
max_concurrent_runs = 2
intake_interval_secs = 120  # GitHub ラベルを読む intake の周期。
                            # キューの権威自体はローカル sqlite

[pr]
draft = true   # PR をドラフトで開く。プロジェクト単位は [projects.pr] で上書き
```

`[projects.pr]` は `[pr]` セクション全体を一括で上書きします（キー単位ではありません）。

### worktree セットアップフック（オプトイン）

`[projects.worktree_setup]` は、meguri が worktree を準備するたびに(初回だけでなく create/attach/re-point のたびに)プロジェクト独自のコマンドを実行します。`attach_worktree`/`create_review_worktree` は再利用時に `reset --hard` + `clean -fd` で untracked なファイルを消すことがあるため、毎回の実行が必要になります。meguri 自身はここで何を実行するかに関与しません(ADR 0003)。apm(「[エージェント向け指示（apm）](#エージェント向け指示apm)」参照)はその一利用例であり、専用の組み込み連携ではありません:

```toml
[projects.worktree_setup]
commands = ["apm install --frozen"]        # sh -c で順に実行。途中で失敗したら以降は実行しない
exclude = [".claude/rules", "AGENTS.md"]   # .git/info/exclude に追記(常時追記される .meguri/ に加えて)
required = false                           # true にすると失敗時に run が失敗扱いになる(既定は warn して続行)
timeout_secs = 300                         # コマンドごとのタイムアウト。ネットワーク fetch を伴い得るため
```

コマンドは worktree を `cwd` として実行され、`MEGURI_ROLE`(run の loop 種別 — `worker`)、`MEGURI_PROFILE`(解決された起動プロファイル)、`MEGURI_ISSUE`(対象の issue/task 番号)が環境変数として渡されます。コマンドは同じ worktree に対して複数回実行され得るため、冪等に書いてください。

meguri 自身のループにこのフックを配線した実例(#139、dogfood 検証込み)は [docs/ops/apm-worktree-setup.md](docs/ops/apm-worktree-setup.md) を参照してください。`apm install --frozen` は毎回 `apm.lock.yaml`(git 追跡ファイル)の `local_deployed_files` を書き換えるため、`commands` に `git checkout -- apm.lock.yaml` を続けて入れないと、エージェント自身が触っていないファイルのせいで clean-tree 検証が落ちます — `exclude` は未追跡ファイルにしか効かないので救えません。

### agent profile（オプトイン）

既定ではすべての run が単一の `[agent]` profile（名前は `default`）で起動します。**名前付き profile**（1 つの CLI の起動バンドル。`[agent]` と同じ形）を定義し、`[[projects]]` の `profile` でプロジェクトごとに選べます:

```toml
[agents.profiles.claude-opus]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "opus"]
resume_args = ["--resume"]
# preflight = []   # 起動時の folder-trust prime をオプトアウト（セキュリティ参照）

[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
resume_args = ["resume"]

[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"
repo_slug = "owner/repo"
profile = "codex"    # 省略で default（[agent]）
```

`claude-opus` / `claude-sonnet` / `codex` は組み込みなので、`[agents.profiles]` を書かずに参照できます。`profile` は定義済みの名前でなければならず、未知の名前は `meguri watch` / `meguri run` の起動時に中断します（黙ってフォールバックしません）。run の最初の pane spawn で選ばれた profile は run に固定され（`meguri ps` の PROFILE 列）、以後の spawn / resume で再利用されます。`meguri doctor` が profile 一覧と各プロジェクトの解決を表示します。

### prompt preamble（`[prompts]`、オプトイン）

「作業前にこのガードレールを読む」「この品質バーを満たさないものは commit しない」のような恒常的なプロジェクト規律は issue ごとではなく全 issue 共通です。`[prompts]` はそれをターンプロンプトに注入します。値は **repo 相対**パスで、ファイルの内容がプロンプト冒頭に埋め込まれます（前置き — 完了契約は常に最後で、そちらが勝ちます）。

```toml
[prompts]                          # トップレベル既定（全プロジェクトに適用）
all = "ops/agents/guardrails.md"   # 共通 preamble
worker = "ops/agents/worker.md"    # worker ループ用

[projects.prompts]                 # per-project override（キー単位）
worker = "ops/agents/worker.md"
```

- **参照ではなく埋め込み** — profile が Claude でも Codex でも、エージェントがファイルを開こうが開くまいが、規律は届きます。
- **`all` → `worker` の順に両方**注入。per-project エントリはトップレベルを**キー単位で**上書きします（未知のキーは設定ロードで中断）。
- **無いのは非致命** — 存在しないパス（や worktree を脱出する symlink）は警告してスキップし、ターンは走ります。
- 常時読み込みで足り、Claude しか使わないなら [エージェント向け指示（apm）](#エージェント向け指示apm) / `CLAUDE.md` で十分です — `[prompts]` は CLI 非依存の配達が要るときに使い、ファイルは短く保ってください。

## 開発

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

テストスイートは、スクリプト化された偽エージェント TUI（`tests/fixtures/fake_agent.sh`）を使い、本物の tmux・本物の git worktree・ローカルの bare origin に対してループ全体を駆動します — blocked ダイアログの処理、嘘をつくエージェントの矯正、検証フィードバック、クラッシュリカバリを含みます。


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

meguri は意図的に**単一のループ**だけを回します: **worker**(キューされた task → 検証済み PR、local mode では検証済みローカルブランチ)。以前のイテレーションは 10 のループ(planner・レビュワー群・fixer 一族・cleaner・triage …)まで肥大し、検証が表面積に追いつかなくなったため、2026-08 に核まで刈り込みました([docs/design/kernel-pruning-plan.md](docs/design/kernel-pruning-plan.md))。削除した機構は休眠 ADR として残っています([docs/adr/STATUS.md](docs/adr/STATUS.md)): それぞれ、解いていた失敗が核の運用で実際に再観測されたときだけ、証拠・出生時の削除条件・人間レビューのゲートを揃えて戻ります。

**バージョニング。** meguri は 1.0 前（`0.x`）で [SemVer](https://semver.org/lang/ja/) に従います: `0.x` の間は public API と CLI が未安定で、minor（`0.y`）が破壊的変更を含みうる一方、patch（`0.y.z`）は互換を保ちます。安定を約束するのは `1.0.0` からです。現在の挙動に依存する場合はバージョンを固定してください。

**リリース。** リリースはタグ駆動です（ADR 0007）: メンテナがバージョンを bump し、`CHANGELOG.md` を更新して `vX.Y.Z` タグを push すると、`.github/workflows/release.yml` が macOS arm64 / Linux x86_64 のバイナリをビルドして GitHub Release に添付し（本文は git-cliff 生成のノート）、（crate 設定が済めば）OIDC Trusted Publishing で crates.io に publish します。**push したタグがそのままリリースの起点**なので、タグは慎重に — 誤タグは誤リリースになります。

## コントリビューション

人間からのバグ報告・PR を歓迎します — 通常の fork & PR フローで、`meguri:*`
ラベルを気にする必要はありません。詳細は [CONTRIBUTING.md](CONTRIBUTING.md)
（英語）を参照してください。

## ライセンス

MIT
