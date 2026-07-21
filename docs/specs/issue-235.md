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

claude の既定 pre-flight は `claude --dangerously-skip-permissions -p 'ok'`。headless `-p` は
唯一ゲートを素通りする経路で、その一回で bypass 受諾（config-dir 単位）とフォルダ信頼（cwd の
パス単位）を CLI 自身が書き残す。フィールド名を meguri が知る必要はない（version-stable）。
代替（JSON 直書き / meguri 所有 config-dir / launch mode 変更）を退けた理由は ADR 0027。

**empirical 検証（実装の最初のステップ、この判断の前提）**: 実機で
`claude --dangerously-skip-permissions -p 'ok'` を一度走らせた後、同 config-dir・同 cwd への
対話起動がゲートに当たらないこと、および doctor の gate-probe が `Clear` になることを確認する。
確認できれば D1 のまま。万一 prime が受諾を永続化しないと判明したら ADR 0027 の rejected 案 2
（meguri 所有 config-dir、資格情報の供給・保護・分離・削除を別 issue で設計）へ切り替える —
その分岐点と条件はここで確定済みで、レビュー後に蒸し返さない。

### D2. config スキーマ = `AgentProfile.preflight: Option<Vec<String>>`

`headless_args` と同じ「完全な argv」流儀（`args` とは融合しない）。解決規則も踏襲する:

- 非空 → そのまま `{command} {preflight}` で prime を実行。
- 明示的な `[]` → pre-flight 無効（opt-out。TOML は `None` を書けないので空配列で表す）。
- 省略 → known-CLI の既定: `claude` → `["--dangerously-skip-permissions", "-p", "ok"]` /
  それ以外（cursor-agent 含む）→ `[]`（cursor-agent は `--trust`/`--force` を `args` で毎回
  素通りするので prime 不要）。

`args` と融合しない理由は `headless_args` と同じで、`args` は yolo とモデルフラグを融合して
運ぶため。prime は「モデルを呼ぶ最小の一撃」であればよく、`args` のモデル選択に依存しない。

### D3. 実行場所 = `spawn_agent_pane` の pane spawn 直前、`Pane` mode のみ

`src/engine/flow.rs:1389` の `spawn_agent_pane` で、`deps.mux.spawn_pane(...)` を呼ぶ直前に
prime を同期実行する。cwd は worktree（フォルダ信頼はパス単位なので必須）、env は pane が使うのと
同じ config-dir を継承（meguri プロセスの環境そのまま — `src/gate.rs` の `pane_effective_config_dir`
と同じ解決）。direct mode（`launch::resolve` が `Direct`）は元々 headless `-p` でゲートに当たらない
ので prime しない。resume の有無に関わらず走らせる（フォルダ信頼は worktree ごとに要る）。

### D4. hang 対策

- **timeout**: `PREFLIGHT_TIMEOUT = 30s`（定数）。prime は実際にモデルを一撃するので gate-probe の
  8s より長い。超過したら kill + reap して pane 起動へ進む（best-effort）。
- **子プロセス回収**: `process_group(0)` で子を独立グループにし、timeout 時は `killpg` →
  `kill(pid)` フォールバック → deadline 付き `try_wait` で回収する。`src/gate.rs` の
  `kill_and_reap_with_deadline` と同型（共通化するか小関数を複製する）。PTY は不要 — `-p` は
  自分で exit するので plain subprocess（stdin=null、stdout/stderr は捨てるかキャプチャ）で足りる。
- **pane 起動前に足す遅延の上限**: prime を同期で待つこと自体が遅延で、その上限が
  `PREFLIGHT_TIMEOUT`。人工的な追加 sleep は入れない。

### D5. 失敗フォールバック（pane を殺さない）

prime の spawn 失敗・非ゼロ終了・timeout のいずれでも pane は殺さず起動する。ゲートは前ターンで
既に満たされているかもしれず、人の attach 導線も残す。prime 失敗が hang より悪い結果を生んでは
ならない（ADR 0027）。

## 触るファイル

- `src/config.rs` — `AgentProfile` に `preflight: Option<Vec<String>>`（`#[serde(default)]`）を追加。
  既定解決は `routing`（`effective_headless_args` の隣）に `effective_preflight_args(profile,
  command)` として置く（headless_args の前例に倣う）。
- `src/preflight.rs`（新規）— prime の実行体。`run_preflight(command, argv, cwd, env, timeout) ->
  PreflightOutcome`。process-group spawn + timeout + reap。`src/gate.rs` の reap ロジックを共有。
- `src/engine/flow.rs` — `spawn_agent_pane` の spawn 直前に `Pane` mode のとき prime を呼ぶ。
  結果を `preflight.ran` / `preflight.failed` イベントで emit。
- `src/lib.rs` — `pub mod preflight;`。
- `docs/adr/0027-profile-preflight-primes-first-run-gate.md` — 決定の記録（本 PR 同梱済み）。
- `README.md` / `README.ja.md` — pre-flight prime の一段落（何を・なぜ・`preflight = []` で無効化）。
- `tests/` — 下記テスト計画。

## migration & rollback（veto: 必須）

- **前捌きが書かせる状態**: `~/.claude.json` の bypass 受諾フィールド（config-dir 単位）と
  フォルダ信頼のパスエントリ（worktree 単位）。**書くのは CLI 本体**で、meguri はこのファイルを
  読まないし書かない。人が一度受諾したのと同じ状態が、自動で作られるだけ。
- **旧 config 互換**: `preflight` は `#[serde(default)]`。既存 config には無いので known-CLI 既定に
  落ちる。claude profile は自動で prime が有効になる（＝ hang を直す望ましい挙動）。これは既存
  ユーザーへの挙動変更だが、pane 起動前に軽い prime を足すだけで厳密に安全側。README で明記する。
- **無効化手段**: profile に `preflight = []` を書けば prime しない（＝旧挙動へ完全復帰）。
  role を `[launch.roles]` で `direct` に倒す経路でも prime は走らない。
- **prime 失敗フォールバック**: D5。pane は殺さず起動。best-effort。
- **rollback**: `preflight = []` で挙動は完全に元通り。`~/.claude.json` に残る受諾状態は CLI 側の
  資産であり meguri の管理外・無害。meguri がそれを掃除する責務は負わない（負えば version-fragile な
  JSON 結合が復活する）。掃除が要るなら CLI 自身の手段でやる、と README に一行。
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

- `src/preflight.rs` 単体: `effective_preflight_args` の解決（claude 既定 / `[]` opt-out / 明示
  argv / unknown command → `[]`）。実行体は seam を切って注入（`src/gate.rs` の PTY launcher 注入と
  同型）— spawn 成功/非ゼロ/timeout の3分岐を実プロセス無しで検証し、timeout+reap の実プロセス経路は
  短い timeout で軽く1本だけ通す（gate.rs の `spawn_pty_probe_with_timeout` テストと同型）。
- `flow.rs` 側: `Pane` mode で prime が呼ばれ、`Direct` mode / `preflight = []` で呼ばれないこと。
  prime 失敗時に pane が起動し続ける（`FakeMux` に spawn_pane が届く）こと。
- 統合（既存の `tests/fixtures/fake_agent.sh` 系）: prime を実行してから pane 起動 → 完了
  コントラクトが返るまでを、prime をスクリプト化した fake CLI で通す（実 claude は叩かない）。

## 受け入れ基準

1. 新規 worktree の `Pane` role 起動で、pane spawn の前に prime が worktree cwd・pane と同じ
   config-dir で一度走る。
2. `preflight = []` の profile / `direct` mode の role では prime が走らない。
3. prime の spawn 失敗・非ゼロ終了・timeout のいずれでも pane は起動する（best-effort）。
4. timeout 超過時、子プロセスは process-group ごと回収され、prime が `PREFLIGHT_TIMEOUT` +
   reap deadline を超えて生き残らない。
5. `preflight` 省略時、claude は既定 prime・cursor-agent は空に解決される。
6. meguri は `~/.claude.json` を読まない・書かない（採用案の不変条件、コードで担保）。
7. README（en/ja）に pre-flight prime と `preflight = []` 無効化が記述される。
8. 既存テストが全部通る。

## スコープ外

- 実機で prime が受諾を永続化しない場合の meguri 所有 config-dir 設計（ADR 0027 rejected 案 2。
  資格情報の供給・保護・分離・削除を含む別 issue）。今回の empirical 検証で不要と確認する前提。
- doctor（#234）への変更。pre-flight が緑にする対象であって、doctor 自体は変えない。
- per-project の `preflight` override。今回は profile 単位で足りる。
