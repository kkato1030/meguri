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

3. **フォルダ信頼はパス単位で `~/.claude.json` に記録される**（実機確認: `hasTrustDialogAccepted`
   が project パスごとに存在）。meguri の worktree は毎回新しいパスなので、fresh worktree は常に
   未信頼＝信頼ダイアログが出る。しかも信頼判定は repo 側 settings を読む**前**に行われるため
   （claude 側の CVE 修正）、worktree 内の `.claude/settings.json` では自分を pre-trust できない。

4. **bypass 受諾を非対話で満たす、supported で version 安定な口は現状ない**（claude-code-guide 調査
   ＋実機確認）。`permissions.defaultMode = "bypassPermissions"` を settings.json に置いても
   対話受諾は別ゲートで消えない。受諾フィールドの名前・場所はバージョンで揺れ（実機の
   growthbook に `tengu_disable_bypass_permissions_mode` すらある＝挙動がリモート切替される）、
   直書きは version-fragile。**唯一素通りするのは headless `-p`**。cursor-agent（grok）は
   `--trust`/`--force` を args で前捌き済み＝同型の別解。

この 4 が、下の 2 分割と「前捌きは設計パスが要る」判断を演繹する。

## 要件カバレッジ（親の受け入れの芯 → 子）

| 親の芯 | 担当 |
|---|---|
| 芯2: doctor が対話起動を反映し、詰まる状態を緑にしない | 子0 |
| 芯1: 人が押さず対話 pane が起動し完了コントラクトを返す（環境非依存・自動） | 子1 |
| 芯3: `~/.claude.json` 内部フィールド直書きに依存しない | 子0・子1 の両方（設計制約） |

## 依存グラフとロールアウト順

```
子0（doctor: false-green を潰す, ready）
  └─ blocks ─▶ 子1（profile pre-flight で自動前捌き, plan）
```

1. **子0 を先に出す。** 単独で「詰まる状態を赤にする」診断価値がある。かつ子1 の受け入れ
   オラクル（前捌き後に doctor が緑になり実ターンも通る、を検証する物差し）になる。
2. **子1 を次に。** 前捌きの中核（headless prime が受諾／信頼 receipt を claude 自身の形式で
   永続化するか等）を設計パスで検証してから実装する。

分割は一段のみ。子1 は「plan」＝自前の実装 spec を書くが、さらに分割はしない。

## 各子の done-criteria

### 子0（ready）— doctor: 対話ゲートの false-green を潰す

- claude probe が headless `-p` の成否だけを緑判定に使うのをやめ、**対話起動で現れる初回ゲート
  （bypass 受諾／フォルダ信頼）を検知**する。ゲートが出る状態は緑にせず、赤＋1行 remediation
  を出す（例: 前捌き未了／該当ゲートの解消方法）。ゲートを越えて ready まで進めば緑。
- 検知は pty 下で `-p` なし・timeout 付き起動しダイアログ文言を照合する経路を基本とする。
  `~/.claude.json` 内部フィールドの**読取り**にも依存しない（書取り同様 version-fragile なので）。
- 対話起動を要するプロファイル（generic probe 側も同じく pane 起動のもの）で同型に効く。
- 既存の probe 分類（`ProbeOutcome::Ok/ModelInvalid/Unavailable`）と doctor 出力・テスト
  （closure 注入で subprocess を張らない現行流儀）を壊さない。
- 永続状態・config スキーマ・public contract に触れない（切り戻しが容易）。

### 子1（plan, blocked_by: 子0）— profile pre-flight で初回対話ゲートを自動前捌き

- pane 起動前に**非対話でゲートを満たす**一般化された per-profile pre-flight を導入する。
  claude の bypass 受諾＋フォルダ信頼、cursor-agent の `--trust` を**同じ枠**で扱う（direction 3）。
- 受け入れ: 新規マシン／新規 config／新規プロファイルで、人が `2` を押さずに対話 pane の agent が
  起動し完了コントラクトを返す。前捌き後は doctor（子0 の検知）が緑になる。
- `~/.claude.json` 内部フィールドの直書きに依存しない（version-fragile 回避）。
- **設計パスで決めるべき A/B（この plan spec で確定させる）**:
  - headless prime（例: `claude --dangerously-skip-permissions -p 'ok'` を pane 起動前に一度
    実行）が bypass 受諾／フォルダ信頼の receipt を **claude 自身の形式で永続化するか** を実機検証。
    するなら、それが version-stable な前捌き（meguri は JSON を触らず claude に書かせる）。
    しないなら別解（`CLAUDE_CONFIG_DIR` を meguri 所有にして prime する／当該 role の launch mode を
    見直す 等）へ。
  - フォルダ信頼はパス単位なので、prime を **worktree の cwd で**走らせて当該パスの信頼を得る設計。
  - config スキーマへ `preflight`（プロファイルの前捌き argv）を追加するかの是非と形。
- **migration & rollback 必須**（veto: public contract＝config スキーマ追加、かつ各マシンの
  config-dir 永続状態に副作用があるため）。前捌きが書く／書かせる状態、旧 config との互換、
  無効化手段、失敗時フォールバック（prime 失敗で pane を殺さない）を plan spec に明記する。
- 前捌きという設計判断は spec より長生きするので、実装時に **ADR** を1本積む。

## 子（machine-readable）

```json meguri-children
[
{"title": "doctor: 対話 pane の初回ゲート false-green を潰す", "body": "## 背景\n\nclaude probe が headless `-p` 固定で叩くため（`src/routing.rs:110` `probe_claude` / `:151` `probe_generic`）、対話起動で現れる初回ゲート（Bypass Permissions mode の受諾ダイアログ、fresh worktree のフォルダ信頼ダイアログ）を素通りする。結果、対話 pane なら詰まる状態でも doctor が緑になる（false-green）。meguri は画面を読まない設計なので、この緑は「実ターンも通る」を保証しない。\n\n## やること\n\n- claude probe を headless の成否だけで緑判定するのをやめ、対話起動で現れる初回ゲートの有無まで検知する。ゲートが出る状態は緑にせず、赤＋1行の remediation を出す。ゲートを越えて ready まで進めば緑。\n- 検知は pty 下で `-p` なし・timeout 付き起動しダイアログ文言を照合する経路を基本とする。`~/.claude.json` の内部フィールドの読取りにも依存しない（書取り同様 version-fragile）。\n- 対話起動を要するプロファイル（generic 側も pane 起動のもの）で同型に効かせる。\n- 既存の `ProbeOutcome` 分類・doctor 出力・closure 注入のテスト流儀を壊さない。\n\n## 受け入れ\n\n- 受諾／信頼ゲートが未了のプロファイルで doctor が赤＋remediation を出す。\n- ゲート解消済みなら緑のまま。\n- 永続状態・config スキーマ・public contract に触れない。\n\n## 関連\n\n- `src/routing.rs:108`（probe_claude）/ `:147`（probe_generic）/ `src/main.rs:999`（doctor_probe）\n- overview.md（画面読み取りで成否判定しない設計前提）", "kind": "ready", "blocked_by": []},
{"title": "profile pre-flight で初回対話ゲートを自動前捌き", "body": "## 背景\n\n対話 pane 起動（worker/planner/fixer/pr-reviewer は ADR 0012 の pane mode）で agent CLI の初回対話ゲートに詰まる。claude は yolo（`--dangerously-skip-permissions`, `src/config.rs:921`）の「Bypass Permissions mode」一度きり受諾＋fresh worktree のフォルダ信頼、cursor-agent は `--trust`/`--force`（args で前捌き済み＝同型の別解）。meguri は画面を読まないので、人が `2` を押さない限り永久に詰まる。\n\n調査事実: bypass 受諾を非対話で満たす supported で version 安定な口は現状なく（settings.json の `permissions.defaultMode` では対話受諾は消えない）、受諾フィールドの名前・場所はバージョンで揺れる。フォルダ信頼はパス単位で `~/.claude.json` に記録され、信頼判定は repo 側 settings を読む前なので worktree 内 settings では pre-trust できない。唯一素通りするのは headless `-p`。\n\n## やること（この issue は plan: まず実装 spec を書く）\n\n- pane 起動前に非対話でゲートを満たす、一般化された per-profile pre-flight を導入する。claude の bypass 受諾＋フォルダ信頼、cursor-agent の `--trust` を同じ枠で扱う。\n- `~/.claude.json` 内部フィールドの直書きに依存しない（version-fragile 回避）。\n\n## 設計パスで確定させる A/B\n\n- headless prime（例 `claude --dangerously-skip-permissions -p 'ok'` を pane 起動前に一度実行）が bypass 受諾／フォルダ信頼の receipt を claude 自身の形式で永続化するかを実機検証。するならそれが version-stable な前捌き（meguri は JSON を触らず claude に書かせる）。しないなら別解（`CLAUDE_CONFIG_DIR` を meguri 所有にして prime／当該 role の launch mode 見直し 等）。\n- フォルダ信頼はパス単位なので prime を worktree の cwd で走らせて当該パスの信頼を得る。\n- config スキーマへ `preflight`（前捌き argv）を追加するかの是非と形。\n\n## 受け入れ\n\n- 新規マシン／新規 config／新規プロファイルで、人が `2` を押さず対話 pane の agent が起動し完了コントラクトを返す（環境非依存・自動）。\n- 前捌き後は doctor（先行 issue の検知）が緑になり、実ターンも通る。\n- `~/.claude.json` 内部フィールド直書きに依存しない。\n\n## 必須セクション（design / veto）\n\n- public contract（config スキーマ追加）かつ config-dir 永続状態への副作用があるため migration & rollback を必須で書く: 前捌きが書く／書かせる状態、旧 config 互換、無効化手段、prime 失敗で pane を殺さないフォールバック。\n- 前捌きの設計判断は ADR を1本積む。\n\n## 関連\n\n- `src/engine/flow.rs:1402`（pane 起動列 `{command} {args} <trigger>`）/ `src/config.rs:649`（AgentProfile）/ ADR 0012（launch mode）/ overview.md", "kind": "plan", "blocked_by": [0]}
]
```
