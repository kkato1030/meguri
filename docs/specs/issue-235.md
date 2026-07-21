# issue-235 spec — profile pre-flight で初回対話ゲートを自動前捌き

対話 pane（planner / worker / fixer / pr-reviewer は ADR 0012 で `Pane`）の初回起動は、
agent CLI の初回ゲートに詰まる。claude なら「Bypass Permissions mode」の一度きり受諾と、
fresh worktree のフォルダ信頼プロンプト。meguri は画面を読まないので、人が `2` を押さない限り
永久に止まる。

この spec の決定は一行で書ける。**pane 起動の直前に、その CLI 自身の headless 起動を worktree の
cwd で一度走らせて（pre-flight prime）、初回ゲートの受諾を CLI 自身の形式で永続化させる。**
meguri は `~/.claude.json` を一切パースも書き込みもしない。設計判断の本体は **ADR 0027**（本 PR
同梱）に置いた。この spec は実装の足場に徹する。

## spec 深度: design tier（理由）

persistent state（`~/.claude.json` の受諾状態）に副作用があり、public contract（config スキーマ
`preflight`）を足し、hang という operational risk を持つ。veto ルールにより migration & rollback は
必須。よって design spec（architecture / alternatives / migration & rollback / observability /
test strategy を含む）で書く。

## 確定した設計判断（A/B をここで閉じる）

### D1. 前捌きの仕組み = headless prime（採用）

claude の pre-flight は profile の launch `args` を鏡写して headless で走らせる（`{args} -p '<no-op>'`。
既定 yolo profile なら具体的には `claude --dangerously-skip-permissions -p '<no-op>'`。詳細と非 yolo
profile の扱いは D2）。headless `-p` は唯一ゲートを素通りする経路で、その一回でフォルダ信頼（cwd の
パス単位）を、yolo なら加えて bypass 受諾（config-dir 単位）を CLI 自身が書き残す。フィールド名を
meguri が知る必要はない（version-stable）。代替（JSON 直書き / meguri 所有 config-dir / launch mode
変更）を退けた理由は ADR 0027。

**empirical 検証（実装の最初のステップ、この判断の前提）**: 実機で
`claude --dangerously-skip-permissions -p '<no-op>'` を一度走らせた後、同 config-dir・同 cwd への
対話起動がゲートに当たらないこと、および doctor の gate-probe が `Clear` になることを確認する。
確認できれば D1 のまま。万一 prime が受諾を永続化しないと判明したら ADR 0027 の rejected 案 2
（meguri 所有 config-dir、資格情報の供給・保護・分離・削除を別 issue で設計）へ切り替える —
その分岐点と条件はここで確定済みで、レビュー後に蒸し返さない。

### D2. config スキーマ = `AgentProfile.preflight: Option<Vec<String>>`（既定は profile の args を鏡写す）

`headless_args` と同じ「完全な argv」流儀。解決規則:

- 非空 → そのまま `{command} {preflight}` で prime を実行。
- 明示的な `[]` → pre-flight 無効（opt-out。TOML は `None` を書けないので空配列で表す）。
- 省略 → known-CLI の既定: `claude` → **`profile.args` + `["-p", PREFLIGHT_NOOP_PROMPT]`** /
  それ以外（cursor-agent 含む）→ `[]`（cursor-agent は `--trust`/`--force` を `args` で毎回
  素通りするので prime 不要）。

**f3 の決定 — 既定 prime は profile の permission 姿勢をそのまま鏡写す（`--dangerously-skip-permissions`
を強制しない）。** `headless_args`（refine）は read-only を保つため `args` と融合しなかったが、prime は
逆に「pane が実際に当たるゲートと同じ姿勢」で走らねば意味がない。よって既定 prime は `args` を再利用する:

- `args = ["--dangerously-skip-permissions"]`（既定の yolo profile）→ prime も yolo で走り、bypass 受諾
  と フォルダ信頼の両方を書かせる。pane が当たるのがまさに bypass ダイアログだから正しい。
- `args = ["--permission-mode", "acceptEdits"]`（非 yolo の安全側設定）→ prime も acceptEdits で走る。
  この姿勢では bypass ダイアログは出ず、**bypass 受諾は書かれない**。ただし fresh worktree の
  フォルダ信頼プロンプトは permission mode に関係なく出るので、prime はフォルダ信頼だけを確立して
  その hang を解く。共有 config-dir へ勝手に bypass を書き込む副作用は起きない。

つまり bypass 受諾が書かれるのは「profile が既に yolo を選んでいる」場合だけで、それは yolo pane が
どのみち引き起こす状態と同一。安全側 profile の意図を prime が裏切ることはない。

`PREFLIGHT_NOOP_PROMPT` は何も変更させない固定文字列（例: `"reply ok and make no changes"`）。それでも
prime は「実モデルターンを1回・worktree cwd で・repo の指示（CLAUDE.md 等）を読んで」実行する副作用を
持つ（トークン消費と1往復）。この副作用は ADR 0027 と README に明記する。

### D3. 実行場所 = `spawn_agent_pane` の pane spawn 直前、`Pane` mode のみ、worktree ごと一度

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

**f2 の決定 — worktree 単位の実行済みマーカーで prime を「一度だけ」に束ねる。** 現行 `ensure_pane`
は resume 失敗時に同じ worktree で plain spawn を再試行するため（`flow.rs` の resume→fallback 経路）、
素の設計だと1回の起動で prime が二重に走る。worktree 内の runtime マーカー
`.meguri/.preflight-done`（git-excluded・worktree と一緒に消える）を導入し、prime 成功時に書く。
prime 実行前にこのマーカーを見て、在れば skip する。resume/plain の両経路が同じマーカーを参照するので、
1回の `ensure_pane` で prime は高々1回。フォルダ信頼状態自体は `~/.claude.json` に永続するので、
daemon 再起動後もマーカー在り＝再 prime 不要で整合する（マーカーが消えても再 prime は冪等で無害）。

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
  既定解決は `routing`（`effective_headless_args` の隣）に `effective_preflight_args(profile,
  command)` として置く。既定は `profile.args` + `["-p", PREFLIGHT_NOOP_PROMPT]`（D2 の鏡写し規則）。
- `src/preflight.rs`（新規）— prime の async 実行体。`run_preflight(command, argv, cwd, config_dir,
  timeout) -> PreflightOutcome`。`tokio::process` + `process_group(0)` + async timeout + reap（D4）。
- `src/gate.rs` — `REAP_DEADLINE` を `pub` にする（preflight と共有）。`pane_effective_config_dir`
  を共通の `effective_config_dir()`（絶対パス化）に寄せる（f1）。
- `src/engine/flow.rs` — `spawn_agent_pane` に (a) `effective_config_dir()` の絶対パスを
  `PaneSpec.env` の `CLAUDE_CONFIG_DIR` に載せる（f1）、(b) `.meguri/.preflight-done` マーカーを見て
  未実行かつ `Pane` mode のとき prime を呼び、成功時にマーカーを書く（f2）、(c) 結果を
  `preflight.ran` / `preflight.failed` イベントで emit。
- `src/lib.rs` — `pub mod preflight;`。
- `docs/adr/0027-profile-preflight-primes-first-run-gate.md` — 決定の記録（本 PR 同梱）。
- `README.md` / `README.ja.md` — pre-flight prime の一段落（何を・なぜ・実モデルターンの副作用・
  `preflight = []` で無効化・非 yolo profile では bypass を書かないこと）。
- `tests/` — 下記テスト計画。

## migration & rollback（veto: 必須）

- **前捌きが書かせる状態**: `~/.claude.json` の フォルダ信頼のパスエントリ（worktree 単位、常に）と、
  **profile が yolo を選んでいる場合のみ** bypass 受諾フィールド（config-dir 単位）。**書くのは CLI
  本体**で、meguri はこのファイルを読まないし書かない。人が一度受諾したのと同じ状態が自動で作られる。
- **f3 の是正 — 「厳密に安全側／完全に元通り／無害」は profile 依存**:
  - yolo profile（`args` に `--dangerously-skip-permissions`）: prime は bypass 受諾を共有 config-dir
    へ書く。これは yolo pane が初回起動でどのみち引き起こす状態と同一なので、prime が新たな危険を
    足すわけではない。ただし rollback（`preflight = []`）後もこの受諾は残る。
  - 非 yolo profile（`args = ["--permission-mode", "acceptEdits"]` 等）: prime は同じ姿勢で走るので
    **bypass 受諾を書かない**。フォルダ信頼だけを書く。安全側設定の意図は保たれる。
  よって旧文言は撤回し、「yolo profile では bypass 受諾が config-dir に残る（yolo pane と同じ状態）／
  非 yolo profile では書かれない」と正確に述べる。
- **旧 config 互換**: `preflight` は `#[serde(default)]`。既存 config には無いので既定に落ちる。
  既定 claude profile は yolo なので prime が有効になる（＝ hang を直す望ましい挙動）。挙動変更である
  ことと、prime が実モデルターンを1回走らせる副作用を README で明記する。
- **無効化手段**: profile に `preflight = []`（prime しない）。role を `[launch.roles]` で `direct` に
  倒す経路でも prime は走らない。
- **prime 失敗フォールバック**: D5。pane は殺さず起動。best-effort。
- **rollback**: `preflight = []` で meguri の挙動（prime を走らせるか）は元通り。ただし既に書かれた
  受諾/信頼状態は `~/.claude.json` に残る（CLI 側の資産・meguri の管理外）。yolo profile の bypass 受諾を
  本当に消したいなら CLI 自身の手段でやる、と README に一行。meguri がその JSON を掃除する責務は負わない
  （負えば version-fragile な JSON 結合が復活する）。
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

- `src/preflight.rs` 単体: `effective_preflight_args` の解決（claude yolo → `args`+`-p`／claude
  非 yolo → bypass を含まない鏡写し／`[]` opt-out／明示 argv／unknown command → `[]`）。実行体は
  seam を切って注入 — spawn 成功/非ゼロ/timeout の3分岐を実プロセス無しで検証。
- **f5 の reap 上限**: 短い timeout の実プロセス経路を1本通し、prime が
  `PREFLIGHT_TIMEOUT + 2×REAP_DEADLINE` を超えて生き残らないこと・子孫が process-group ごと回収される
  ことを検証（gate.rs の `spawn_pty_probe_with_timeout` テストと同型）。
- **f4 の非ブロッキング**: prime を timeout に張り付かせている間に、別の async タスク（例: 早く返る
  ダミー run）が同じ runtime 上で進むことを `tokio` テストで検証（worker を塞がない証拠）。
- **f1 の config-dir 一致**: `FakeMux`（tmux/herdr 両相当）で spawn した `PaneSpec.env` の
  `CLAUDE_CONFIG_DIR` が prime に渡した絶対パスと一致すること。相対 `CLAUDE_CONFIG_DIR` を与えても
  両者が同じ絶対パスに正規化されることを1本で確認。
- **f2 の一度だけ**: `ensure_pane` の resume 失敗→plain spawn 再試行で prime が二重に走らないこと
  （マーカーで skip）。`Direct` mode / `preflight = []` で prime が呼ばれないこと。prime 失敗時に
  pane が起動し続ける（`FakeMux` に spawn_pane が届く）こと。
- 統合（既存の `tests/fixtures/fake_agent.sh` 系）: prime を実行してから pane 起動 → 完了
  コントラクトが返るまでを、prime をスクリプト化した fake CLI で通す（実 claude は叩かない）。

## 受け入れ基準

1. 新規 worktree の `Pane` role 起動で、pane spawn の前に prime が worktree cwd で **一度だけ** 走り、
   その `CLAUDE_CONFIG_DIR` は pane の `PaneSpec.env` に載る絶対パスと一致する（f1）。resume 失敗→
   plain 再試行でも二重に走らない（f2、マーカーで skip）。
2. `preflight = []` の profile / `direct` mode の role では prime が走らない。
3. prime の spawn 失敗・非ゼロ終了・timeout のいずれでも pane は起動する（best-effort）。
4. timeout 超過時、子プロセスは process-group ごと回収され、prime が `spawn_agent_pane` を塞ぐのは
   高々 `PREFLIGHT_TIMEOUT + 2×REAP_DEADLINE`（= 34s）まで。この間も他の run は同じ runtime 上で
   進む（f4、async 非ブロッキング）。
5. `preflight` 省略時、yolo claude profile は `args`+`-p` に解決され bypass 受諾＋信頼を書く。
   非 yolo profile は bypass を含まない鏡写しに解決され信頼のみ書く。cursor-agent 等は空（f3）。
6. meguri は `~/.claude.json` を読まない・書かない（採用案の不変条件、コードで担保）。
7. README（en/ja）に pre-flight prime・実モデルターンの副作用・`preflight = []` 無効化・非 yolo profile
   では bypass を書かないことが記述される。
8. 既存テストが全部通る。

## スコープ外

- 実機で prime が受諾を永続化しない場合の meguri 所有 config-dir 設計（ADR 0027 rejected 案 2。
  資格情報の供給・保護・分離・削除を含む別 issue）。今回の empirical 検証で不要と確認する前提。
- doctor（#234）への変更。pre-flight が緑にする対象であって、doctor 自体は変えない。
- per-project の `preflight` override。今回は profile 単位で足りる。
