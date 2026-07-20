# issue-232: 対話 pane の初回受諾ゲート対策 — 分割提案

<!-- meguri:decompose-proposal -->

## なぜ実装 spec ではなく分割提案か

この issue は一本の実装 spec に収束しない。中身が「確実で安全ですぐ効く半分」と
「肝心の仕組みが未検証で設計パスが要る半分」に、はっきり割れているからだ。

- **doctor の false-green を潰す**（issue の「最低限」）は、対象も直し方もほぼ確定していて、
  永続状態にもスキーマにも触らない。単独で価値があり、単独で切り戻せる。
- **対話ゲートの自動前捌き**は、中核の仕組み（後述）が実機検証を要し、config スキーマ
  （public contract）と各マシンの config-dir 永続状態に触る。設計判断（ADR）を伴う。

「別々の PR としてレビューし、別々に切り戻したいか？」— 答えは Yes。よって分割する。
片方を待たせて他方の不確実性に巻き込まない。前捌き側の未確定な A/B（仕組みの選択）を、
無理に今の一本 spec へ畳んで「後で決める」を残すこともしない。

## 親のゴール

新規マシン／新規 config／新規プロファイルでも、**人が手で `2`（Yes, I accept）を押さずに**
対話 pane の agent が起動し完了コントラクトを返せること。かつ doctor が「対話起動が通るか」を
反映し、詰まる状態を緑にしないこと。`~/.claude.json` の内部フィールド直書きに依存しない。

## 調査で分かった事実（設計の土台）

1. **pane の起動列は `{command} {args} <trigger>`**（`src/engine/flow.rs:1402`）。
   worker/planner/fixer/pr-reviewer は `pane` mode（ADR 0012）＝生きた対話セッションで起動し、
   既定 args は yolo（`--dangerously-skip-permissions`, `src/config.rs:921`）。ここで claude の
   「Bypass Permissions mode」一度きり受諾ダイアログに当たる。

2. **doctor の probe は headless `-p` 固定**（`src/routing.rs:110` `probe_claude`／`:151`
   `probe_generic`）。headless は受諾ダイアログもフォルダ信頼ダイアログも**出さずに素通り**する。
   だから「対話なら詰まる状態」でも probe は成功＝緑になる（false-green）。

3. **2つのゲートはスコープが違う。** bypass 受諾は **config-dir（マシン）単位**で永続化される
   ＝どの worktree でも同じ。一方フォルダ信頼は **worktree のパス単位**で `~/.claude.json` に
   記録される（実機確認: `hasTrustDialogAccepted` が project パスごと）。meguri の worktree は
   毎回新しいパスなので fresh worktree は常に未信頼。しかも信頼判定は repo 側 settings を読む
   **前**に行われるため（claude 側の CVE 修正）、worktree 内の `.claude/settings.json` では
   自分を pre-trust できない。**このスコープ差が、下の決定 D1 を決める。**

4. **bypass 受諾を非対話で満たす、supported で version 安定な口は現状ない**（claude-code-guide 調査
   ＋実機確認）。`permissions.defaultMode = "bypassPermissions"` を settings.json に置いても
   対話受諾は別ゲートで消えない。受諾フィールドの名前・場所はバージョンで揺れ（実機の
   growthbook に `tengu_disable_bypass_permissions_mode` すらある＝挙動がリモート切替される）、
   直書きは version-fragile。**唯一素通りするのは headless `-p`**。cursor-agent（grok）は
   `--trust`/`--force` を args で前捌き済み＝同型の別解。

この 4 が、下の 2 分割・3 決定と「前捌きは設計パスが要る」判断を演繹する。

## 確定した決定（self-review の A/B を閉じる）

- **D1（gate のスコープで役割を割る / finding f1）。** doctor は「毎回同じ＝マシン単位」の
  **bypass 受諾ゲートだけ**を検知する。フォルダ信頼は worktree のパス単位で、doctor 実行時には
  当該 worktree がまだ存在せず probe は doctor の cwd を継承するため、そもそも検知できない。
  よってフォルダ信頼は **子1 の launch-time 前捌き（worktree cwd で prime）が唯一の担保**とし、
  子0 では扱わない。この結果「doctor 緑 → 実ターンも通る」の正確な線引きは:
  *doctor はマシン可搬な bypass ゲートを保証し、per-worktree のフォルダ信頼は子1 が自動化して
  人手ゲートを消す*（doctor は未作成パスを前もって検証しない）。子0 は**子1 の bypass 半分の
  緑オラクル**になる（folder-trust 半分は実ターン進行が担保）。

- **D2（doctor は pane 到達 profile だけを gate-probe / finding f2）。** 「対話起動を要するか」は
  profile ではなく role の launch mode の性質で、同じ profile が pane role と direct role の両方から
  使われうる。よって子0 は `launch::resolve`（role→mode）と `routing::resolve`（role→profile）を
  消費し、**いずれかの pane-mode role が解決する profile 集合だけ**を、`(command, config-dir,
  gating args)` で重複排除して gate-probe する。direct 専用 profile はゲートに当たらないので対象外。
  既存の per-profile `--version`／model probe ループ（profile 列挙）は**意味を変えず不変**、
  gate 検知は launch 情報を受け取る別パスとして足す。

## 要件カバレッジ（親の受け入れの芯 → 子）

| 親の芯 | 担当 |
|---|---|
| 芯2: doctor が対話起動を反映し、詰まる状態を緑にしない | 子0（bypass ゲート。D1 によりフォルダ信頼は対象外） |
| 芯1: 人が押さず対話 pane が起動し完了コントラクトを返す（環境非依存・自動） | 子1（bypass ＋フォルダ信頼の両方を前捌き） |
| 芯3: `~/.claude.json` 内部フィールド直書きに依存しない | 子0・子1 の両方（設計制約） |

## 依存グラフとロールアウト順

```
子0（doctor: bypass ゲートの false-green を潰す, ready）
  └─ blocks ─▶ 子1（profile pre-flight で自動前捌き, plan）
```

1. **子0 を先に出す。** 単独で「bypass で詰まる状態を赤にする」診断価値がある。かつ子1 の
   受け入れオラクル（前捌き後に doctor の bypass 検知が緑になる、を検証する物差し）になる。
2. **子1 を次に。** 前捌きの中核（headless prime が受諾／信頼 receipt を claude 自身の形式で
   永続化するか等）を設計パスで検証してから実装する。フォルダ信頼は D1 により子1 が唯一の担保。

分割は一段のみ。子1 は「plan」＝自前の実装 spec を書くが、さらに分割はしない。

## 各子の done-criteria

### 子0（ready）— doctor: bypass ゲートの false-green を潰す

- **対象は config-dir 単位の bypass 受諾ゲートのみ**（D1）。フォルダ信頼は per-worktree で
  doctor 時に検証不能なため子0 では扱わない（子1 の launch-time 前捌きが担保）。
- **launch 情報で対象を絞る**（D2）。`launch::resolve` ＋ `routing::resolve` から pane-mode role が
  解決する profile 集合を作り、`(command, config-dir, gating args)` で重複排除して gate-probe する。
  既存の per-profile version／model probe は不変。
- **gate probe は pty 下で `-p` なし・timeout 付き起動**し、`~/.claude.json` 内部フィールドの
  **読取り**にも依存しない（書取り同様 version-fragile）。
- **結果は3値**（`ProbeOutcome` とは別の gate 用型を新設 / f3）:
  - ready 文言を**積極検知**した時のみ → ✅ 緑。
  - 既知の bypass ゲート文言を検知 → ❌ 赤・fatal・1行 remediation。
  - timeout／未知出力／spawn 失敗 → ⚠️ **非緑・非 fatal**。
- **PTY 起動部は closure で注入できる seam** にし、単体テストで: ゲート検知→Blocked、
  ready→Clear、timeout→Inconclusive、spawn 失敗→Inconclusive を検証（現行の closure 注入流儀）。
- **hang と副作用の封じ込め**（f4）: PTY 子孫を含む **process group を必ず終了・回収**する
  （対話 CLI は自然終了しない）。受諾入力は送らず**永続状態を変えない**。端末バッファを
  ログに出さない。
- **失敗側規則**（f4）: 緑は ready の積極一致がある時だけ。ゲート文言が変わって一致しなくなった
  状態は ⚠️（非緑）に落ち、決して緑にしない — 同じ false-green が文言変更で再発しない。
- 永続状態・config スキーマ・public contract に触れない（切り戻しが容易）。

### 子1（plan, blocked_by: 子0）— profile pre-flight で初回対話ゲートを自動前捌き

- pane 起動前に**非対話でゲートを満たす**一般化された per-profile pre-flight を導入する。
  claude の bypass 受諾＋フォルダ信頼、cursor-agent の `--trust` を**同じ枠**で扱う（direction 3）。
  **フォルダ信頼は D1 により子1 が唯一の担保**（doctor では検知できない）。
- 受け入れ: 新規マシン／新規 config／新規プロファイルで、人が `2` を押さずに対話 pane の agent が
  起動し完了コントラクトを返す。前捌き後は doctor（子0）の bypass 検知が緑になる。
- `~/.claude.json` 内部フィールドの直書きに依存しない（version-fragile 回避）。
- **設計パスで決めるべき A/B（この plan spec で確定させる）**:
  - headless prime（例: `claude --dangerously-skip-permissions -p 'ok'` を pane 起動前に一度
    実行）が bypass 受諾／フォルダ信頼の receipt を **claude 自身の形式で永続化するか** を実機検証。
    するなら、それが version-stable な前捌き（meguri は JSON を触らず claude に書かせる）。
    しないなら別解（`CLAUDE_CONFIG_DIR` を meguri 所有にして prime する／当該 role の launch mode を
    見直す 等）へ。
  - フォルダ信頼はパス単位なので、prime を **worktree の cwd で**走らせて当該パスの信頼を得る設計。
  - config スキーマへ `preflight`（プロファイルの前捌き argv）を追加するかの是非と形。
  - **hang 対策**（f5）: pre-flight の timeout、PTY 子孫を含む子プロセス回収、pane 起動前に足す
    遅延の上限。
- **migration & rollback 必須**（veto: public contract＝config スキーマ追加、かつ各マシンの
  config-dir 永続状態に副作用があるため）。前捌きが書く／書かせる状態、旧 config との互換、
  無効化手段、失敗時フォールバック（prime 失敗で pane を殺さない）を plan spec に明記する。
- **`CLAUDE_CONFIG_DIR` を meguri 所有にする案を残す場合の追加 veto 論点**（f5）: その config-dir で
  認証情報をどう**供給・保護・profile 間で分離・削除**するか。失敗時フォールバックだけでは、
  hang（timeout＋回収が要）と資格情報の副作用（分離・削除が要）は扱えない。
- 前捌きという設計判断は spec より長生きするので、実装時に **ADR** を1本積む。

## 子（machine-readable）

```json meguri-children
[
{"title": "doctor: 対話 pane の bypass 受諾ゲートの false-green を潰す", "body": "## 背景\n\nclaude probe が headless `-p` 固定で叩くため（`src/routing.rs:110` `probe_claude` / `:151` `probe_generic`）、対話起動で現れる初回ゲート（Bypass Permissions mode の受諾ダイアログ）を素通りする。結果、対話 pane なら詰まる状態でも doctor が緑になる（false-green）。meguri は画面を読まない設計なので、この緑は「実ターンも通る」を保証しない。\n\n## スコープの決定（親 spec D1）\n\n本 issue が扱うのは config-dir（マシン）単位で永続化される bypass 受諾ゲートのみ。フォルダ信頼は worktree のパス単位で、doctor 実行時には当該 worktree が未作成・probe は doctor の cwd を継承するため検知できない。フォルダ信頼は後続の pre-flight issue が launch 時に担保する（本 issue の対象外）。よって本 issue は『bypass 受諾ゲートの緑オラクル』になる。\n\n## やること\n\n- **対象を launch 情報で絞る（親 spec D2）**: `launch::resolve`（role→launch mode）と `routing::resolve`（role→profile）から、いずれかの pane-mode role が解決する profile 集合を作り、`(command, config-dir, gating args)` で重複排除して gate-probe する。direct 専用 profile はゲートに当たらないので対象外。既存の per-profile `--version`／model probe ループは意味を変えず不変で、gate 検知は launch 情報を受け取る別パスとして足す。\n- **gate probe**: pty 下で `-p` なし・timeout 付き起動し、既知ダイアログ文言を照合する。`~/.claude.json` の内部フィールドの読取りにも依存しない（書取り同様 version-fragile）。\n- **結果は3値（`ProbeOutcome` とは別の gate 用型を新設）**: ready 文言を積極検知した時のみ ✅ 緑／既知 bypass ゲート文言を検知したら ❌ 赤・fatal・1行 remediation／timeout・未知出力・spawn 失敗は ⚠️ 非緑・非 fatal。\n- **hang と副作用の封じ込め**: PTY 子孫を含む process group を必ず終了・回収する（対話 CLI は自然終了しない）。受諾入力は送らず永続状態を変えない。端末バッファをログに出さない。\n- **失敗側規則**: 緑は ready の積極一致がある時だけ。ゲート文言が変わって一致しなくなった状態は ⚠️（非緑）に落ち決して緑にしない — 文言変更で同じ false-green を再発させない。\n- **seam とテスト**: PTY 起動部を closure で注入できる seam にし（現行の closure 注入流儀）、単体テストで ゲート検知→Blocked、ready→Clear、timeout→Inconclusive、spawn 失敗→Inconclusive を検証する。\n\n## 受け入れ\n\n- bypass 受諾が未了の pane 到達 profile で doctor が赤＋remediation を出す。\n- 受諾済みなら緑のまま。フォルダ信頼は本 issue の対象外。\n- 永続状態・config スキーマ・public contract に触れない。\n\n## 関連\n\n- `src/routing.rs:108`（probe_claude）/ `:147`（probe_generic）/ `src/main.rs:779`（doctor_agents）・`:999`（doctor_probe）\n- `src/launch.rs`（launch::resolve）/ `src/routing.rs`（routing::resolve）/ ADR 0012（launch mode）\n- overview.md（画面読み取りで成否判定しない設計前提）", "kind": "ready", "blocked_by": []},
{"title": "profile pre-flight で初回対話ゲートを自動前捌き", "body": "## 背景\n\n対話 pane 起動（worker/planner/fixer/pr-reviewer は ADR 0012 の pane mode）で agent CLI の初回対話ゲートに詰まる。claude は yolo（`--dangerously-skip-permissions`, `src/config.rs:921`）の「Bypass Permissions mode」一度きり受諾＋fresh worktree のフォルダ信頼、cursor-agent は `--trust`/`--force`（args で前捌き済み＝同型の別解）。meguri は画面を読まないので、人が `2` を押さない限り永久に詰まる。\n\n調査事実: bypass 受諾を非対話で満たす supported で version 安定な口は現状なく（settings.json の `permissions.defaultMode` では対話受諾は消えない）、受諾フィールドの名前・場所はバージョンで揺れる。フォルダ信頼はパス単位で `~/.claude.json` に記録され、信頼判定は repo 側 settings を読む前なので worktree 内 settings では pre-trust できない。唯一素通りするのは headless `-p`。\n\n## スコープの決定（親 spec D1）\n\nbypass 受諾（config-dir 単位）は先行の doctor issue が緑オラクルになる。フォルダ信頼は per-worktree で doctor では検知できないため、本 issue の launch-time 前捌き（worktree cwd で prime）が唯一の担保。\n\n## やること（この issue は plan: まず実装 spec を書く）\n\n- pane 起動前に非対話でゲートを満たす、一般化された per-profile pre-flight を導入する。claude の bypass 受諾＋フォルダ信頼、cursor-agent の `--trust` を同じ枠で扱う。\n- `~/.claude.json` 内部フィールドの直書きに依存しない（version-fragile 回避）。\n\n## 設計パスで確定させる A/B\n\n- headless prime（例 `claude --dangerously-skip-permissions -p 'ok'` を pane 起動前に一度実行）が bypass 受諾／フォルダ信頼の receipt を claude 自身の形式で永続化するかを実機検証。するならそれが version-stable な前捌き（meguri は JSON を触らず claude に書かせる）。しないなら別解（`CLAUDE_CONFIG_DIR` を meguri 所有にして prime／当該 role の launch mode 見直し 等）。\n- フォルダ信頼はパス単位なので prime を worktree の cwd で走らせて当該パスの信頼を得る。\n- config スキーマへ `preflight`（前捌き argv）を追加するかの是非と形。\n- hang 対策: pre-flight の timeout、PTY 子孫を含む子プロセス回収、pane 起動前に足す遅延の上限。\n\n## 受け入れ\n\n- 新規マシン／新規 config／新規プロファイルで、人が `2` を押さず対話 pane の agent が起動し完了コントラクトを返す（環境非依存・自動）。\n- 前捌き後は doctor（先行 issue）の bypass 検知が緑になり、フォルダ信頼は実ターン進行が担保、実ターンも通る。\n- `~/.claude.json` 内部フィールド直書きに依存しない。\n\n## 必須セクション（design / veto）\n\n- public contract（config スキーマ追加）かつ config-dir 永続状態への副作用があるため migration & rollback を必須で書く: 前捌きが書く／書かせる状態、旧 config 互換、無効化手段、prime 失敗で pane を殺さないフォールバック。\n- `CLAUDE_CONFIG_DIR` を meguri 所有にする案を残す場合は、その config-dir で認証情報をどう供給・保護・profile 間で分離・削除するかも veto 論点に含める。失敗時フォールバックだけでは hang（timeout＋回収が要）と資格情報の副作用（分離・削除が要）を扱えない。\n- 前捌きの設計判断は ADR を1本積む。\n\n## 関連\n\n- `src/engine/flow.rs:1402`（pane 起動列 `{command} {args} <trigger>`）/ `src/config.rs:649`（AgentProfile）/ ADR 0012（launch mode）/ overview.md", "kind": "plan", "blocked_by": [0]}
]
```
