# ADR 0013: config を 4 store に分類する — desired state(spec 軸)と engine config を分け、precedence × partition で键粒度を閉じる

- Status: proposed
- Date: 2026-07-23
- Issue: #225(#199 を合流)

> 番号について: 本リポジトリの ADR 番号は一意ではなく slug と Issue 番号で区別する
> (ADR 0012 参照)。本 ADR は ADR 0012「承認後の動き」がスライス 5 に割り当てた
> `0013`(config 键粒度)を、config-store の slug で採用する。

## Context

ADR 0012 で meguri は level-triggered reconciler へ移った。その決定 5 で、**ラベル 2 軸
(`meguri:*`)と issue 本文は「望ましい状態(desired state)の宣言 = reconciler への入力」**
だと再解釈された。つまり forge のラベル・本文は、実は **config の一種**(engine が観測して
従う設定値)である。ところが従来 meguri の config 議論は `~/.meguri/config.toml` と repo の
`meguri.toml`(ADR 0011 / #165)しか視野に入れておらず、この「desired state という第4の
置き場」を config の地図に載せていなかった。

同時に、engine config 側の键粒度(どのキーがどこに書けるか)は決定が散らばっている:

- ADR 0011 は「repo 可 = プロジェクト内在の事実だけ」という**境界原理**と
  `builtin < host global < repo < host [projects.*]` の **4 層 precedence** を与えた。
  だが具体分類は「初期は `check_command` / `language` / `pr.draft` だけ、`clean` /
  `prompts`(#149)/ `worktree_setup`(#139)/ `schedules`(#146)は将来」と**保留付き**で、
  以後キーが増えるたび(`cadence` #148、`triage` per-project、`notify` #205 …)場当たりに
  `[projects.*]` へ足されてきた。
- その結果、「プロジェクト内在の事実」なのに host `config.toml` にしか書けないキーが増殖し、
  ADR 0011 が正した歪み(repo 内在の設定が repo の外に住む)が再発している。

ADR 0011 が `clean` を保留した理由は「cleaner loop は run flow に乗らず、claim 時 pin 機構が
届かない」ことだった。だが**スライス 1〜4 で cleaner / triage は Repo Kind reconciler の
observe を持った**。repo-scope の読み取り点が生まれた今、保留の前提は消えている。

この機を捉えて config の地図を書き直す。**4 store に分類し、desired state と engine config を
分け、键粒度を「precedence × partition」で一意に閉じる。**

## Decision

### 1. 4 store

config の置き場を 4 つに分類する。前 3 つ(A/B/C)は **engine config**(engine の振る舞い・
権限・リソースの宣言)、4 つ目(D)は **desired state**(engine が寄せていく目標)である。

- **Store A — host エンジン設定**(`~/.meguri/config.toml` の top-level セクション)。
  ホスト運用者のリソース・権限・チューニング。`mux` / `agent(s)` / `routing` / `drift` /
  `escalation` / `launch` / `collab` / `limits` / `scheduler` / `daemon` /
  `notifications`(webhook)/ `decompose` / `reconcile` / `reconciler`、および
  `language` / `pr` / `clean` / `triage` の**グローバル既定**。
- **Store B — 信頼境界の宣言**(`~/.meguri/config.toml` の `[[projects]]` + `[[workspaces]]`)。
  config への登録そのものが信頼行為(ADR 0011)。project identity・マシン/token 束縛・
  信頼の宣言(`id` / `repo_slug` / `repo_path` / `mode` / `deliver` / `default_branch` /
  `worktree_root` / `autonomy` / `review` / `pr.auto_merge`)と、Store C 可キーの
  **host 上書き**(`[projects.*]`、precedence 最上位)を持つ。
- **Store C — repo `meguri.toml`**。プロジェクト内在の事実を repo 自身が宣言する
  (ADR 0011 の境界原理)。code と一緒に versioned で、どのホストで回しても一致する。
- **Store D — forge ラベル + issue 本文**。desired state = spec 軸(ADR 0012 決定 5)。
  `meguri:plan` / `meguri:ready`(phase)・`meguri:hold` / `meguri:needs-human`(人間制御)・
  `meguri:automerge`(opt-in)・issue 本文(タスク payload)。**人間・上流が「こうあって
  ほしい」と書き込む面**であり、engine は観測して従うが、ここを engine config から再構築
  しない(ADR 0012)。

「desired state と engine config の分離」とは **D と {A, B, C} の分離**である。A/B/C は
「engine をどう動かすか」、D は「engine に何をさせたいか」。両者は precedence 上まざらない
(下記 3)。

### 2. partition — 各キーはちょうど1つの「所有 store」を持つ

キーごとに、その値が**どの store で権威になるか(所有 store)**と、**どの store が
書けるか(eligibility)**を定める。ADR 0012 の「全状態にちょうど1つの所有 arm」と同じ
精神で、**全 config キーはちょうど1つの所有 store に属し、所有の欠落も二重所有も無い**
ことを不変条件にする(コードで totality を担保 = 下記 実装)。

section 内でも键粒度は section 単位に固定しない。既に `[pr]`(`draft` は C 可、`auto_merge`
は A/B)が section 内キー単位境界の先例(ADR 0011)。本 ADR はこれを一般化し、以下の
**8 箇所の切り直し**で partition を境界原理に一致させる(切り直し = 現状の粒度が原理と
食い違う箇所の是正):

1. **`prompts`**(#149): host-only → **Store C 可**(claim 時 pin)。repo 相対の preamble
   パスは repo 内在。ADR 0011 が「将来 repo 可」と名指ししたもの。
2. **`worktree_setup`**(#139): host-only → **Store C 可**(worktree 準備 = claim 時に読み pin)。
   repo のセットアップ手順は repo 内在。
3. **`plan_delivery`**: host-only → **Store C 可**(claim 時 pin)。「この repo の spec を
   separate / combined どちらで届けるか」は repo 内在。
4. **`clean`**: host-only(ADR 0011 保留)→ **Store C 可**(Repo Kind observe 時に読む)。
   保留の前提だった「読み取り点が無い」はスライス 1〜4 で解消した。
5. **`triage` の键単位分割**(`[pr]` に次ぐ 2 例目): `ignore` は **Store C 可**(誤検知抑制は
   repo 内在)。`mode` / `apply` / `confidence_threshold` / `max_actions_per_tick` は
   **A/B 専用** — spec 軸(Store D)への**書き込み権限**の宣言であり、信頼・自律度の決定
   (ADR 0017)だから repo に委ねない。
6. **`cadence`**(#148): host-only → **Store C 可**(discover 時に読む)。ラベル別消化レート
   上限は repo 内在。
7. **`notify` の键単位分割**: `[projects.notify] labels`(どのラベルを watch するか)は
   **Store C 可**(repo 内在の関心)。`[notifications]` webhook は **Store A 専用**
   (ホストのリソース・秘密)。C の selector が A のリソースに乗る形。
8. **`schedules` の読み機構の切り直し**(#146 / ADR 0015): 所有は **Store C** だが、読みは
   claim 時 pin ではなく **default branch から fire 時に読む**。これで **Store C が持つ読み
   機構は「claim 時 pin / observe 時 / discover 時 / default-branch fire 時」の 4 種**だと
   明示する。所有 store(= partition)と読み機構は独立の軸である。

上記以外は決着済みとして表に固定する(§Consequences の分類表)。

### 3. precedence — 所有 store が複数層に現れるときの勝ち順

ADR 0011 の 4 層を partition 対応に一般化する。**engine config(A/B/C)の precedence**:

```
builtin 既定 < Store A(host global)< Store C(repo meguri.toml, 読み機構ごとに pin)< Store B(host [projects.*] 上書き)
```

host(B)が最後に勝つ原則は不変 — 運用者はいつでもローカルで矯正できる(ADR 0011)。

**Store D は engine config の precedence 鎖に入らない**。D は desired state(入力)であり、
A/B/C は engine config(振る舞い)なので、両者は同じ軸で優先度を争わない。engine は D を
**observe** し、A/B/C の設定に従って D を望ましい状態へ寄せる。「`hold` と
`[reconciler] policy` のどちらが勝つか」のような比較は起きない — 前者は「何をしたいか」、
後者は「どう動くか」で、直交する。

### 4. 混入はエラー、壊れた設定でプロセスを殺さない(不変)

Store C の eligibility は `meguri.toml` スキーマの `deny_unknown_fields` が enforce する
(ADR 0011)。A/B 専用キー(`repo_slug` / `[agent]` / `triage.mode` / …)を `meguri.toml` に
書けば parse error、`meguri doctor` が報告する。parse/検証に失敗した `meguri.toml` は
warn + `repo_config.invalid` emit の上で「無いもの扱い」にフォールバックする(不変)。

## Consequences

### 分類表(partition の総体 — これが「閉じている」の実体)

| store | キー | 読み機構 |
|---|---|---|
| A(host engine) | `mux` `agent(s)` `routing` `drift` `escalation` `launch` `collab` `limits` `scheduler` `daemon` `notifications` `decompose` `reconcile` `reconciler`；`language`/`pr`/`clean`/`triage` の global 既定 | hot reload(process-bound を除く) |
| B(信頼境界) | `[[projects]]` `[[workspaces]]`；`id` `repo_slug` `repo_path` `mode` `deliver` `default_branch` `worktree_root` `autonomy` `review` `pr.auto_merge`；C 可キーの `[projects.*]` 上書き | hot reload |
| C(repo meguri.toml) | `check_command` `language` `pr.draft` `prompts`※ `worktree_setup`※ `plan_delivery`※ `clean`※ `triage.ignore`※ `cadence`※ `notify.labels`※ `schedules` | claim 時 pin / observe 時 / discover 時 / default-branch fire 時 |
| D(desired state) | `meguri:plan` `meguri:ready` `meguri:hold` `meguri:needs-human` `meguri:automerge` `meguri:working`/`speccing`/`implementing`(status 軸)；issue 本文 | reconciler observe(ADR 0012) |

※ = 本 ADR の 8 切り直しで Store C 可に是正、または键単位で分割したキー。

- config の地図が 4 store で閉じる。新キーは「どの store の所有か・どの読み機構か」を
  最初に答えるだけでよく、場当たりの `[projects.*]` 増殖が止まる。
- プロジェクト内在の設定が repo と一緒に versioned になる範囲が広がる(prompts /
  worktree_setup / clean / cadence …)。ADR 0011 の狙いが完成する。
- desired state(D)が config の地図に第一級で載り、「ラベル/本文は engine config を
  上書きするのか?」という混乱が構造的に消える(直交、precedence で争わない)。
- **totality を守る責務**が生まれる。`ProjectConfig` にフィールドを足すたび、その所有
  store を宣言し、C 可なら `meguri.toml` スキーマ側にも足す。これを**テストで機械的に
  守る**(所有の欠落 = ADR 0012 の BEHIND 類の config 版)。

### hot reload 非回帰

Store A/B は従来どおり `ConfigReloader` で hot reload(process-bound の `mux.kind` /
`mux.session` / `[daemon]` / `[collab]` は pin、不変)。Store C は hot reload されず、
**読み機構ごとに pin/read される**(claim 時 pin は run 中不変 = ADR 0011 のセキュリティ芯)。
8 切り直しで C 可になったキーも、host `[projects.*]`(B)側の hot reload は従来どおり効く
(B が最後に勝つ)。「repo 化したら host 側の hot 編集が効かなくなる」回帰は起きない。

### rejected

- **D を precedence 鎖に入れる**(ラベルと engine config を同じ優先度で比較): desired state と
  engine config を混同する。`hold`(何をしたいか)と `policy`(どう動くか)は直交で、勝ち負けを
  争う関係ではない。
- **section 単位で store を固定する**(`[triage]` 丸ごと host、`[notify]` 丸ごと C 等): spec 軸
  書き込み権限(triage.mode)と誤検知抑制(triage.ignore)は信頼レベルが違う。键単位境界が要る。
- **repo(C)を host(B)より優先させる**: 運用者がローカルで矯正できなくなる(ADR 0011 不変)。
- **`clean` を再び保留する**: Repo Kind observe という読み取り点がスライス 1〜4 で生まれた今、
  保留の技術的前提が無い。分類を閉じる好機を逃す理由がない。
