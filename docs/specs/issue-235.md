# issue-235 spec — profile pre-flight で初回対話ゲートを自動前捌き

対話 pane（planner / worker / fixer / pr-reviewer は ADR 0012 で `Pane`）の初回起動は、
agent CLI の初回ゲートに詰まる。claude なら「Bypass Permissions mode」の一度きり受諾と、
fresh worktree のフォルダ信頼プロンプト。meguri は画面を読まないので、人が `2` を押さない限り
永久に止まる。

この spec の決定は一行で書ける。**pane 起動の直前に、その CLI 自身の headless 起動を対象 cwd で一度
走らせて（pre-flight prime）、fresh worktree のフォルダ信頼を CLI 自身の形式で永続化させる。** bypass
受諾は doctor（#234）＋人間の一度きり受諾の担当で、prime は書かせない（親 spec D1）。meguri は
`~/.claude.json` を一切パースも書き込みもしない。設計判断の本体は **ADR 0027**（本 PR 同梱）に置いた。
この spec は実装の足場に徹する。

## spec 深度: design tier（理由）

persistent state（`~/.claude.json` のフォルダ信頼）に副作用があり、public contract（config スキーマ
`preflight`）を足し、hang という operational risk を持つ。veto ルールにより migration & rollback は
必須。よって design spec（architecture / alternatives / migration & rollback / observability /
test strategy を含む）で書く。

## 確定した設計判断（A/B をここで閉じる）

### D1. 前捌きの仕組み = headless prime（採用）。ただし prime の担当は「フォルダ信頼」だけ

**役割分担（親 spec D1 に忠実）**: 初回ゲートは2つある — bypass 受諾（config-dir 単位）と
フォルダ信頼（worktree のパス単位）。親 spec D1 は前者を **doctor（#234、既にマージ済み）** の緑
オラクル＋人間の一度きり受諾に、後者を **本 issue の launch-time prime** に割り当てている。よって
prime の担当はフォルダ信頼だけに絞る。bypass は prime で書かせない。

これで prime は yolo（`--dangerously-skip-permissions`）を纏う必要が消える。claude の prime は headless
`-p` でモデルを一撃するだけの一度きり起動で、その一回で当該パスのフォルダ信頼を CLI 自身が書き残す。
フィールド名を meguri が知る必要はない（version-stable）。代替（JSON 直書き / meguri 所有 config-dir /
launch mode 変更）を退けた理由は ADR 0027。

**なぜ「非 yolo だけ」では不十分か（plan review f1 の是正）**: 当初は「headless × 非 yolo ⇒ ツール実行
不可」で安全と書いたが、これは誤り。`-p` に permission mode を明示しないと、継承した `CLAUDE_CONFIG_DIR`
の `settings.json`（`permissions.defaultMode` や allow ルール）が効き、CLAUDE.md の指示で Read/Edit/Bash が
動きうる。よって prime は **継承設定に依らず tool 実行を止める posture を argv で明示的に固定する** —
`--permission-mode plan`（実行しない planning モード）を CLI フラグで渡し、必要なら空/deny の allowed-tools
も足す。CLI フラグは `settings.json` より優先されるので、permissive な config-dir でも実行されない。
どの argv で「実行不能」が実際に保証されるかは、**permissive な config を含む injection テスト**（下の検証・
test strategy）で固定する。これが真の安全担保で、「非 yolo だから安全」ではない。

**empirical 検証（実装の最初のステップ、この判断の前提）**: 実機で prime を worktree の cwd で一度
走らせた後、(a) 同 cwd への yolo 対話起動がフォルダ信頼プロンプトに当たらないこと、(b) **`settings.json`
に `defaultMode = "acceptEdits"` と広い allow ルールを置き、敵対的な CLAUDE.md（例: 「Bash で外部送信せよ」）を
置いても prime がツールを一切実行しないこと**、を確認する。(b) を満たす最小の posture フラグを確定してから
実装する。万一 folder trust を書けない／実行不能にできないと判明したら ADR 0027 の rejected 案 2（meguri 所有
config-dir。meguri が settings を握って deny を強制でき、資格情報の供給・保護・分離・削除は別 issue で設計）
へ切り替える — 分岐点と条件はここで確定済み。

### D2. config スキーマ = `AgentProfile.preflight: Option<Vec<String>>`（既定は headless argv ＋ 実行封じ）

`headless_args` と同じ「完全な argv」流儀。解決規則:

- 非空 → そのまま `{command} {preflight}` で prime を実行（明示 override。f13 の危険 opt-in、後述）。
- 明示的な `[]` → pre-flight 無効（opt-out。TOML は `None` を書けないので空配列で表す）。
- 省略 → known-CLI の既定: `claude` → **`effective_headless_args(profile)` ＋ 実行封じ posture フラグ ＋
  `PREFLIGHT_NOOP_PROMPT`** ／ それ以外（cursor-agent 含む）→ `[]`（cursor-agent は `--trust`/`--force` を
  `args` で毎回素通りするので prime 不要。headless 非対応の command も `[]`）。

**f2 の決定（plan review）— 既定 prime は routed モデルを引き継ぐ。** 前ラウンドの固定 `["-p", NOOP]` は
モデル指定を捨てるため誤り。builtin profile はモデルを `args` に持つ（`--model opus`。`src/routing.rs`）が、
pane と別モデルで prime すると、CLI 既定モデルが無効・未認証のとき prime が非ゼロ終了 → claim-once の
`failed` マーカーで再試行されず、pane が folder-trust gate で hang して元の問題が残る。そこで **既定 prime は
`effective_headless_args` を土台にする** — これは既に「モデルは保つが yolo は載せない」argv で
（builtin は `["-p", "--model", "opus"]`。refine が read-only なのと同じ理由でモデルだけ引き継ぐ）、pane と
同じモデルを使う。arg 文字列を自前でパースしてモデルを抜き出すより、既存の解決関数に乗る方が堅い。

**f1 の決定（plan review）— その上に「実行封じ posture」を明示的に足す。** `effective_headless_args` は
非 yolo だが permission mode を明示しないので、継承 config 次第でツールが動きうる（D1）。よって prime argv は
`effective_headless_args` に加えて `--permission-mode plan`（実行しない）等の posture フラグを **必ず** 足す。
CLI フラグは `settings.json` より優先されるので継承設定を上書きする。最終形（`plan` だけで足りるか、空/deny
allowed-tools も要るか）は permissive-config injection テストで固定する（test strategy / 受け入れ基準5）。

- **実機検証に失敗した場合の扱い**: (a) 実行封じが効かない、または (b) この posture で folder trust を
  書けない、と判明したら、prime を諦めて meguri 所有 config-dir 案（ADR 0027 rejected 案 2。meguri が
  settings を握って deny を強制）へ切り替える（別 issue）。yolo を prime に足して誤魔化す道は選ばない
  （injection 面を復活させるため）。
- **prime 失敗時の扱い（f2 の後半）**: 上のモデル引き継ぎで「既定モデル未認証で失敗」という f2 の原因は
  消える。それでも prime が失敗（spawn/非ゼロ/timeout）したら claim-once で `failed` を記録し再試行しない
  ——D5 で pane は起動し、gate が残れば既存の pane hang → needs-human ＋ doctor が捕まえる。`failed` マーカーは
  理由を残し運用者が原因を追える。毎 spawn の 34 秒再 prime で埋めない方針は round2 f8 のまま。

`PREFLIGHT_NOOP_PROMPT` は何も変更させない固定文字列（例: `"reply ok and make no changes"`）。prime は
実モデルターンを1回・cwd で・repo の指示（CLAUDE.md 等）を読んで走らせる副作用を持つ（トークン消費と
1往復）。安全性は「非 yolo だから」ではなく **明示した実行封じ posture ＋ それを固定する injection テスト** で
担保する。この副作用と安全性の根拠は ADR 0027 と README に明記する。

**f13 の決定 — 明示 `preflight` override は「安全策のない危険な opt-in」として明示し、警告する。** 既定は
上のとおり実行封じ posture 付きで安全だが、host config が非空 `preflight` を明示すると argv はそのまま実行され、
既定の実行封じ（および非 yolo）縛りを迂回できる。例えば `preflight = ["--dangerously-skip-permissions",
"-p", "ok"]` と書けば、外部 PR の CLAUDE.md を読んだ実モデルが pane 起動前に Bash/Edit 等を承認付きで
動かせる。これを黙って許さない:

- override に `--dangerously-skip-permissions`（または既知の yolo 相当フラグ）が含まれる場合、config
  ロード時に警告ログを出す（`launch::validate` 同様の起動時チェック）。ブロックはしない（host は信頼境界の
  内側で、意図的な上級者 opt-in を潰さない — ADR 0011 の「信頼の宣言は host 専用」に沿う）が、危険であることを
  明示する。README にも「明示 override は自己責任・injection 無防備」と書く。
- なぜ強制封じ（全 preflight に安全 argv を注入）にしないか: `preflight` は `headless_args` 同様
  「完全な argv をそのまま使う」契約で、meguri が勝手に引数を足し引きすると override の意味が壊れる。
  既定を安全にし、逸脱は host の明示的判断＋警告で扱うのが契約に忠実。判断は ADR 0027 に記録。

### D3. 実行場所 = pane spawn 直前（worker/advisor 両サイト）、`Pane` mode のみ、identity＋パスごと一度

`src/engine/flow.rs` の `spawn_agent_pane` で、`deps.mux.spawn_pane(...)` を呼ぶ直前に prime を
**async で** 実行する（f4 の詳細は D4）。cwd は worktree（フォルダ信頼はパス単位なので必須）。
direct mode（`launch::resolve` が `Direct`）は元々 headless `-p` でゲートに当たらないので prime しない。

**f1 の決定 — 実効 config-dir を絶対パスに解決し、prime と `PaneSpec.env` の両方へ明示的に渡す。**
現行の tmux/herdr は生きたサーバー経由で pane を作るため、pane はサーバーが起動時に握った環境を使い、
prime は watch/daemon プロセスの環境を使う。両者で `CLAUDE_CONFIG_DIR` が違ったり相対指定だったりすると、
prime の receipt が pane の読む場所と別の config-dir に書かれ、pane は同じ初回ゲートで止まる。よって:

- `effective_config_dir()` を1か所に定義: `CLAUDE_CONFIG_DIR`（あれば絶対パス化。相対なら daemon の
  cwd 基準で正規化）／無ければ `~/.claude`（絶対）。`src/gate.rs` の `pane_effective_config_dir` は
  この共通関数に寄せる（doctor と launch で解決を一致させる）。
- その絶対パスを prime の env（`CLAUDE_CONFIG_DIR`）と `PaneSpec.env` の両方へ明示的に載せる。
  `PaneSpec.env` は tmux（`src/mux/tmux.rs:128`）も herdr（`src/mux/herdr.rs:454`）も pane に
  注入するので、サーバーが握っていた環境に依らず両 mux で同一の config-dir に揃う。

**f2 / f6 / f7 / f8 の決定 — 「済み」状態を gate identity 単位で持ち、直列化し、一度で打ち切る。**
現行 `ensure_pane` は resume 失敗時に同じ worktree で plain spawn を再試行する（`flow.rs` の
resume→fallback 経路）ため、素の設計だと1回の起動で prime が二重に走る。これを次の3点で束ねる:

- **identity 単位のマーカー（f6）**: 状態を command 非依存の単一フラグにすると、同じパスで先に走った
  cursor-agent（または別 command）のマーカーが後続 claude の必要な prime を握り潰す。そこでマーカーを
  **gate identity + 対象パス** 単位にする — キーは `(command, 実効 config_dir, preflight argv, 絶対対象パス)`
  （`gate.rs` の `GateTarget` の dedup キーにパスを足したもの）。別 command / 別パスは別マーカーになり
  互いを skip しない。
- **マーカーの置き場所（f11）**: マーカーを prime する cwd の中（worktree や advisor_dir）に置くと、
  advisor_dir は `spawn_advisor` のたびに削除・再作成されるためマーカーも毎回消え、re-embodiment ごとに
  prime が再実行されて claim-once が壊れる。よってマーカーは **ephemeral な cwd の外**、meguri 所有の
  安定領域 `~/.meguri/preflight/<hash>`（`config::meguri_home()`、hash = 上記キーの短縮ハッシュ）に置く。
  worktree/advisor_dir の寿命に依らず一度きりが保たれる。`~/.meguri` は既存の state root（DB・worktrees と
  同じ親）で `MEGURI_HOME` で移せる。
- **直列化（f7）**: 確認→prime→書き込みが原子的でも直列でもないと、parallel self-review の複数
  reviewer が同じパスで同時に初回 spawn したとき全員がマーカー未作成を見て実モデル prime を重複実行する。
  プロセス内の非同期ロック（上記キーをキーにした `HashMap<Key, Arc<tokio::sync::Mutex<()>>>`）で critical
  section（マーカー確認→prime→記録）を直列化する。先着がロックを取って prime し、後着はロック解放後に
  マーカーを見て skip する（先着の prime 完了を待ってから自分の pane を起動するので、後着の pane はゲートに
  当たらない）。別パス・別 identity は別キーなので並行して走れる。複数 daemon プロセスの同時起動は通常
  構成外としスコープ外。
- **claim-once（f8）**: マーカーを成功時だけ書くと、spawn 失敗・非ゼロ・timeout の後は次の spawn
  ごとに最大 34 秒の prime を繰り返す。よって prime を試みたら結果（`success` / `failed:<reason>`）を
  マーカーに記録し、identity+パスごとに二度は試みない。`success` なら skip。`failed` でも自動
  再試行しない（`preflight.failed` を残し pane は D5 で起動）。prime 失敗でゲートが残るなら、それは
  既存の pane hang → needs-human 経路と doctor が捕まえる問題で、毎 spawn の再 prime で埋めない。
  これで D3/受け入れ基準1の「identity＋パスごと一度だけ」が数の上でも保証される。

**f9 の決定 — advisor pane も同じ machinery で prime する。** `spawn_advisor_pane`（`flow.rs`）も
fresh な `advisor-{issue}` ディレクトリで同じ対話 CLI を直接起動するので、初回フォルダ信頼ゲートが残り
advisor が無人で停止する。よって advisor の pane spawn 直前にも同じ prime（非 yolo `-p` folder-trust
prime・identity＋パスマーカー・直列化・claim-once・失敗フォールバック）を入れる。cwd は advisor_dir、
マーカーは f11 のとおり `~/.meguri/preflight/` 側なので advisor_dir 再作成でも消えない。advisor dir は
repo を持たない空ディレクトリなので prime が読む指示すら無く、injection 面は元々生じない。

### D4. hang 対策（f4: async・runtime を塞がない / f5: reap を数値で確定）

**f4 の決定 — `tokio::process` + async timeout。Tokio worker を塞がない。** prime は最大 30 秒
待つので、これを同期で `spawn_agent_pane`（async）から呼ぶと worker thread を占有し、並列 run・
scheduler・crash recovery が同じ runtime 上で止まる。`src/refine.rs:61` の既存パターン
（`tokio::process::Command` + `tokio::time::timeout` + `kill_on_drop(true)`）に倣う:

- `tokio::process::Command`、`process_group(0)`、stdin=null、stdout/stderr は捨てる、`kill_on_drop(true)`。
  PTY は不要（`-p` は自分で exit する）。
- `tokio::time::timeout(PREFLIGHT_TIMEOUT, child.wait())` で待つ。全て `.await` なので、待機中も
  他の run/loop は同じ runtime 上で進む。
- **timeout**: `PREFLIGHT_TIMEOUT = 30s`（定数）。prime は実モデルターンなので gate-probe の 8s より長い。

**f5 の決定 — reap は `gate.rs` の `REAP_DEADLINE` をそのまま共有し、最終動作まで数値で固定。**
`gate::REAP_DEADLINE`（= 2s）を `pub` にして preflight から import する（gate.rs は同期 std::process、
preflight は async tokio::process で process-model が違うため helper 関数は共有できないが、**定数と
最終動作は同一**に揃える）。timeout 時の回収手順:

1. `killpg(pid, SIGKILL)` で子と全子孫を一撃（`process_group(0)` で独立グループにしてある）。
2. `tokio::time::timeout(REAP_DEADLINE, child.wait())` で回収を待つ。
3. 未回収なら `kill(pid, SIGKILL)` を一度だけ再送し、もう一度 `timeout(REAP_DEADLINE, child.wait())`。
4. それでも未回収なら zombie として諦め、pane 起動へ進む（daemon は止めない。`preflight.failed` に記録）。

したがって prime が `spawn_agent_pane` を塞ぐ上限は **`PREFLIGHT_TIMEOUT + 2 × REAP_DEADLINE`
（= 30 + 4 = 34 秒）** で、受け入れ基準4はこの数値で検証できる。人工的な追加 sleep は入れない。

### D5. 失敗フォールバック（pane を殺さない）

prime の spawn 失敗・非ゼロ終了・timeout のいずれでも pane は殺さず起動する。ゲートは前ターンで
既に満たされているかもしれず、人の attach 導線も残す。prime 失敗が hang より悪い結果を生んでは
ならない（ADR 0027）。

## 触るファイル

- `src/config.rs` — `AgentProfile` に `preflight: Option<Vec<String>>`（`#[serde(default)]`）を追加。
  **struct literal 移行（plan review f3）**: `#[serde(default)]` は TOML の後方互換にだけ効き、Rust の
  struct literal には効かない。`..Default::default()` を使わず全フィールドを書く literal は新フィールド
  追加でコンパイル不能になるので、全て更新する — `src/routing.rs`（builtin `claude-opus`/`claude-sonnet`
  ＋その他 builtin、`290`/`307`/`323` 付近、および test の `1022` の `base` closure）、`src/main.rs:1120`、
  `tests/doctor_probe_test.rs:27`、`tests/pr_reviewer_test.rs:724`。（`..Default::default()` を使う
  `src/routing.rs:682`・`tests/worker_test.rs:1204`・`src/agent_session.rs:96` は自動で追従するので変更不要。）
- `src/routing.rs` — 既定解決 `effective_preflight_args(profile, command)` を `effective_headless_args`
  の隣に置く。既定は `claude` → `effective_headless_args(profile)`（モデル引き継ぎ・非 yolo）＋ 実行封じ
  posture フラグ ＋ `PREFLIGHT_NOOP_PROMPT`、他 → `[]`（D2、f1/f2）。加えて明示 override が yolo フラグを
  含む場合の起動時警告（f13、`launch::validate` 同様の一括チェック）。
- `src/preflight.rs`（新規）— prime の async 実行体 + 「一度だけ」の統制。`run_preflight(command,
  argv, cwd, config_dir, timeout) -> PreflightOutcome`（`tokio::process` + `process_group(0)` +
  async timeout + reap、D4）。加えてキー `(command, config_dir, argv, 絶対対象パス)` の ensure 関数:
  `~/.meguri/preflight/<hash>` マーカー（f11、`config::meguri_home()` 配下）の確認、プロセス内 async
  ロックでの直列化、claim-once の記録（f6/f7/f8）。
- `src/gate.rs` — `REAP_DEADLINE` を `pub` にする（preflight と共有）。`pane_effective_config_dir`
  を共通の `effective_config_dir()`（絶対パス化）に寄せる（f1）。
- `src/engine/flow.rs` — `spawn_agent_pane` と `spawn_advisor_pane`（f9）の両方に (a)
  `effective_config_dir()` の絶対パスを `PaneSpec.env` の `CLAUDE_CONFIG_DIR` に載せる（f1）、(b)
  pane spawn 直前に prime の ensure 関数を呼ぶ（`spawn_agent_pane` は `Pane` mode のとき／advisor は
  常に pane）、(c) 結果を `preflight.ran` / `preflight.failed` イベントで emit。
- `src/lib.rs` — `pub mod preflight;`。
- `docs/adr/0027-profile-preflight-primes-first-run-gate.md` — 決定の記録（本 PR 同梱）。
- `README.md` / `README.ja.md` — pre-flight prime の一段落（何を・なぜ・prime はフォルダ信頼だけを担い
  bypass は doctor の担当であること・実モデルターンの副作用・`preflight = []` で無効化・明示 override は
  injection 無防備の自己責任 opt-in であること）。
- `tests/` — 下記テスト計画。

## migration & rollback（veto: 必須）

- **前捌きが書かせる状態**: 既定 prime は `~/.claude.json` の **フォルダ信頼のパスエントリ**（対象パス
  単位）だけを書かせる。bypass 受諾は書かない（doctor＋人間の一度きり受諾の担当。D1）。**書くのは CLI
  本体**で、meguri はこのファイルを読まないし書かない。人がフォルダ信頼プロンプトに一度答えたのと同じ
  状態が自動で作られるだけ。加えて meguri 自身の `~/.meguri/preflight/<hash>` マーカー（済み記録）。
- **安全性（前ラウンドの是正を確定）**: 既定 prime は yolo を纏わないので、共有 config-dir へ bypass を
  勝手に書き込む副作用は **どの profile でも起きない**。安全側 profile の意図を裏切らない。前ラウンドで
  残していた「yolo profile では bypass が残る」という副作用そのものが、既定 prime から yolo を外したこと
  （D1/D2）で消える。bypass を書きたい host は明示 override で自己責任 opt-in できる（f13、警告付き）。
- **旧 config 互換**: `preflight` は `#[serde(default)]`。既存 config には無いので既定に落ち、claude
  profile は自動で folder-trust prime が有効になる（＝ per-worktree の hang を直す望ましい挙動）。挙動変更で
  あることと、prime が実モデルターンを1回走らせる副作用（非 yolo・ツール実行なし）を README で明記する。
- **無効化手段**: profile に `preflight = []`（prime しない）。role を `[launch.roles]` で `direct` に
  倒す経路でも prime は走らない。
- **prime 失敗フォールバック**: D5。pane は殺さず起動。best-effort。
- **rollback**: `preflight = []` で meguri の挙動（prime を走らせるか）は元通り。既に書かれたフォルダ信頼は
  `~/.claude.json` に残る（CLI 側の資産・無害な path trust・meguri の管理外）。`~/.meguri/preflight/` の
  マーカーは消してよい（再 prime は冪等）。meguri は `~/.claude.json` を掃除しない（負えば version-fragile な
  JSON 結合が復活する）。
- **資格情報の副作用**: 採用案（D1）は既定 `~/.claude` をそのまま継承するので、meguri は資格情報を
  供給・保護・分離・削除しない（触らない）。この論点は ADR 0027 rejected 案 2（meguri 所有
  config-dir）に対してのみ立つもので、採用案では発生しない。

## observability（veto tier: 必須）

- `preflight.ran`: `{ profile, command, cwd, duration_ms, exit_status }`（成功時。captured 出力は
  ログに載せない — profile args に秘密が乗りうるのと同じ理由、`src/gate.rs` の f4 参照）。
- `preflight.failed`: `{ profile, command, reason: "spawn"|"timeout"|"nonzero", duration_ms }`。
  pane は続けて起動するので、これは警告であって致命ではない。
- `pane.spawned` イベントに `preflight` の要否/結果を1フィールド足すかは実装時判断（任意）。

## test strategy

- `src/preflight.rs` 単体: `effective_preflight_args` の解決 — **claude → `effective_headless_args`
  （builtin なら `--model` を含む）＋ 実行封じ posture フラグ ＋ `PREFLIGHT_NOOP_PROMPT` で、
  `--dangerously-skip-permissions` を含まず、pane と同じモデル flag を保つ（f2）／`[]` opt-out／明示 argv は
  そのまま／unknown command → `[]`**。実行体は seam を切って注入 — spawn 成功/非ゼロ/timeout の3分岐を
  実プロセス無しで検証。
- **f1 の実行不能（安全ゲート・実機）**: `settings.json` に `defaultMode = "acceptEdits"` と広い allow
  ルールを置いた config-dir で、敵対的 CLAUDE.md（ツール実行を促す）を置いた cwd に prime を走らせ、
  ツールが一切実行されない（＝ posture フラグが継承設定を上書きした）ことを検証する。この test が実行封じ
  posture の最終 argv を固定する。単体側は「解決後 argv に実行封じフラグが必ず載る」ことを引数レベルで確認。
- **f3 の struct literal**: 既存 literal 全更新後に `cargo build`/`cargo test` が通ること（新フィールド
  追加でのコンパイル回帰防止。受け入れ基準8に含む）。
- **f5 の reap 上限**: 短い timeout の実プロセス経路を1本通し、prime が
  `PREFLIGHT_TIMEOUT + 2×REAP_DEADLINE` を超えて生き残らないこと・子孫が process-group ごと回収される
  ことを検証（gate.rs の `spawn_pty_probe_with_timeout` テストと同型）。
- **f4 の非ブロッキング**: prime を timeout に張り付かせている間に、別の async タスク（例: 早く返る
  ダミー run）が同じ runtime 上で進むことを `tokio` テストで検証（worker を塞がない証拠）。
- **f1 の config-dir 一致**: `FakeMux`（tmux/herdr 両相当）で spawn した `PaneSpec.env` の
  `CLAUDE_CONFIG_DIR` が prime に渡した絶対パスと一致すること。相対 `CLAUDE_CONFIG_DIR` を与えても
  両者が同じ絶対パスに正規化されることを1本で確認。
- **f2 の一度だけ**: `ensure_pane` の resume 失敗→plain spawn 再試行で prime が二重に走らないこと。
  `Direct` mode / `preflight = []` で prime が呼ばれないこと。prime 失敗時に pane が起動し続ける
  （`FakeMux` に spawn_pane が届く）こと。
- **f6 の identity 分離**: 同じパスで別 command（例 cursor-agent）が先に記録した後も、claude の初回
  spawn が **なお prime を走らせる**こと（別マーカー）。
- **f7 の直列化**: 同じパス・同じ identity で複数の pane を同時に初回 spawn すると prime がちょうど
  1回だけ走ること（後着はロック後に skip）。
- **f8 の claim-once**: prime を失敗させた後、同じ identity＋パスの次の spawn が prime を再実行しない
  こと（`failed` マーカーで打ち切り）。
- **f9 の advisor**: `spawn_advisor_pane` が pane spawn の前に advisor_dir で prime を1回走らせること。
- **f11 の advisor 再起動**: 同じ advisor を複数回 `spawn_advisor`（advisor_dir 削除・再作成を挟む）
  しても prime が **1回だけ** で、2回目以降は `~/.meguri/preflight/` のマーカーで skip されること。
- **f12 の既定 argv**: 解決後の claude 既定 prime argv が `["-p", PREFLIGHT_NOOP_PROMPT]` に一致し
  `--dangerously-skip-permissions` を含まないこと（安全性の回帰防止）。
- **f13 の危険 override**: `preflight` に yolo フラグを含む明示値を与えると config ロード時に警告が
  出ること（ブロックはしない）。
- 統合（既存の `tests/fixtures/fake_agent.sh` 系）: prime を実行してから pane 起動 → 完了
  コントラクトが返るまでを、prime をスクリプト化した fake CLI で通す（実 claude は叩かない）。

## 受け入れ基準

1. 新規 worktree の `Pane` role 起動で、pane spawn の前に prime が worktree cwd で **identity＋パス
   ごと一度だけ** 走り、その `CLAUDE_CONFIG_DIR` は pane の `PaneSpec.env` に載る絶対パスと一致する
   （f1）。resume 失敗→plain 再試行でも二重に走らない（f2）。同パスで別 command が先に走っても claude は
   別マーカーで prime される（f6）。同時初回 spawn でも prime はちょうど1回（f7）。prime 失敗後は同
   identity＋パスで再試行しない（f8）。
2. `preflight = []` の profile / `direct` mode の role では prime が走らない。`spawn_advisor_pane` も
   同じ prime 経路を通り（f9）、同じ advisor を複数回起動（advisor_dir 削除・再作成）しても prime は
   1回だけ（f11、マーカーは `~/.meguri/preflight/` 側なので消えない）。
3. prime の spawn 失敗・非ゼロ終了・timeout のいずれでも pane は起動する（best-effort）。
4. timeout 超過時、子プロセスは process-group ごと回収され、prime が `spawn_agent_pane` を塞ぐのは
   高々 `PREFLIGHT_TIMEOUT + 2×REAP_DEADLINE`（= 34s）まで。この間も他の run は同じ runtime 上で
   進む（f4、async 非ブロッキング）。
5. `preflight` 省略時、claude は `effective_headless_args`（pane と同じモデル flag を保つ）＋ 実行封じ
   posture フラグ ＋ `PREFLIGHT_NOOP_PROMPT` に解決され、`--dangerously-skip-permissions` を含まない（f2）。
   permissive な `settings.json`（`acceptEdits` ＋ allow）と敵対的 CLAUDE.md の下でも prime はツールを
   一切実行しない（f1、実行封じ posture が継承設定を上書き）。cursor-agent 等は空。明示 `preflight` に yolo
   フラグを含めると config ロード時に警告が出る（f13、ブロックはしない）。
6. meguri は `~/.claude.json` を読まない・書かない（採用案の不変条件、コードで担保）。bypass 受諾は
   doctor＋人間の一度きり受諾の担当で、prime は書かない（D1）。
7. README（en/ja）に prime はフォルダ信頼だけを担い bypass は doctor 担当であること・実モデルターンの
   副作用（非 yolo・ツール実行なし）・`preflight = []` 無効化・明示 override は injection 無防備の自己責任
   opt-in であること（f13）が記述される。
8. 既存テストが全部通る。

## スコープ外

- 実機で prime が受諾を永続化しない場合の meguri 所有 config-dir 設計（ADR 0027 rejected 案 2。
  資格情報の供給・保護・分離・削除を含む別 issue）。今回の empirical 検証で不要と確認する前提。
- doctor（#234）への変更。pre-flight が緑にする対象であって、doctor 自体は変えない。
- per-project の `preflight` override。今回は profile 単位で足りる。
- 複数 daemon プロセスが同一 worktree を同時に初回 spawn する構成（f7 の直列化はプロセス内。通常は
  単一 daemon）。cross-process の prime 重複は idempotent なので実害は無く、必要になれば marker への
  flock で後日ハードニングする。
