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
  `worktree_root` / `autonomy` / `review` / `plan_delivery` / `pr.auto_merge`)と、Store C 可
  キーの **host 上書き**(`[projects.*]`、precedence 最上位)を持つ。
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

**C-eligibility の判定条件(読み取り点の規律)**。あるキーを Store C 可にできるのは、その
**権威ある読み取り点がすべて claim 後**(run が worktree を持ち `with_repo_config` で fold 済み)
**か、default branch 読み**(下記 #8 の共有機構)で賄えるときだけである。claim 前・fold 前・
ambient Deps(run に紐づかない scheduler の Deps)から読まれるキーを C にすると、repo が
`meguri.toml` で宣言しても一部の経路(reconciler の pre-claim snapshot・pr_reviewer の park
判定・worktree 準備フック)だけ host 値のまま残り、経路間で設定が食い違う。したがって
**「読み取り点が claim 後 or default-branch に揃うか」が partition を決める**(所有 store を
「内在の事実か」だけで決めない)。

section 内でも键粒度は section 単位に固定しない。既に `[pr]`(`draft` は C 可、`auto_merge`
は A/B)が section 内キー単位境界の先例(ADR 0011)。本 ADR はこれを一般化し、以下の
**8 箇所の切り直し**で partition を境界原理 + 上記読み取り点条件に一致させる(切り直し =
現状の粒度が原理と食い違う箇所の是正)。各項に、この slice で物理的に移すか(land)、分類は
確定しつつ読み機構の実装を後続に回すか(staged)を明記する:

1. **`prompts`**(#149): host-only → **Store C 可 / land**。preamble は turn プロンプト構築時
   = fold 後の run `deps` から読む(`pr.draft` と同じ post-claim 読み)。読み取り点条件を
   満たすので**本 slice で物理移設**する。repo 相対の preamble パスは repo 内在。
2. **`worktree_setup`**(#139): **Store C 可 / staged**。repo 内在だが、現状 `worktree_setup`
   は `prepare_worktree` の中で **repo_config を読む前**(fold 前)に走る。物理移設には
   「checkout 直後に `meguri.toml` を読んでから setup を走らせる」読み順の変更が要るため、
   分類は C に確定しつつ実装は後続へ。
3. **`plan_delivery`**: **Store B のまま(C にしない)**。`is_combined` は reconciler の
   pre-claim snapshot・pr_reviewer の park 判定・fixer 家族の `pr_is_touchable` から
   **ambient Deps で読まれる**。これらは run 以前・worktree 以前の topology 判定(branch の
   所有・spec PR の gating・human park)で、run の claim pin を共有できない。上記読み取り点
   条件を満たさないので **host 信頼境界(B)に留める**。この項が読み取り点条件の適用例。
4. **`clean`**: **Store C 可 / staged**。ADR 0011 の保留(読み取り点なし)はスライス 1〜4 の
   Repo Kind observe で解消したが、observe は run を持たないので読みは **default branch 読み**
   (#8)を要する。分類は C、実装は後続。
5. **`triage` の键単位分割**(`[pr]` に次ぐ 2 例目): `ignore` は **Store C 可 / staged**
   (誤検知抑制は repo 内在、読みは clean と同じ default-branch)。`mode` / `apply` /
   `confidence_threshold` / `max_actions_per_tick` は **A/B 専用** — spec 軸(Store D)への
   **書き込み権限**の宣言で、信頼・自律度の決定(ADR 0017)だから repo に委ねない。
6. **`cadence`**(#148): **Store C 可 / staged**。ラベル別消化レート上限は repo 内在だが、
   discover(task source)は run 前なので読みは default-branch を要する。分類 C、実装後続。
7. **`notify` の键単位分割**: `[projects.notify] labels` は **Store C 可 / staged**(repo 内在の
   関心、issue 作成時 = run 外の読み)。`[notifications]` webhook は **Store A 専用**
   (ホストのリソース・秘密)。C の selector が A のリソースに乗る形。
8. **`schedules` の読み機構 = 共有 default-branch 読み**(#146 / ADR 0015): 所有は **Store C**、
   読みは claim pin ではなく **default branch から fire 時に読む**(既存実装)。これを
   **run 外 C キーの共有読み機構**として位置づけ、#2/#4/#5/#6/#7 が物理移設するときに再利用
   する。**Store C の読み機構は「claim 時 pin(post-fold)/ default-branch 読み」の 2 系統**で、
   所有 store(partition)と読み機構は独立軸である。

**この slice で物理的に land するのは #1(`prompts`)のみ**。残りは分類(partition)を確定し、
読み機構の実装は明示的に staged にする(ADR 0011 が `clean` を保留した先例と同じ運び)。
「切り直しが 4 store 分類に沿って閉じている」= **分類が総体で閉じている**(全キーの所有 store が
一意に決まり欠落も二重も無い)であって、全キーが同時に `meguri.toml` へ移ることではない。
上記以外は決着済みとして表に固定する(§Consequences の分類表)。

### 3. precedence — 所有 store が複数層に現れるときの勝ち順

ADR 0011 の 4 層を partition 対応に一般化する。**engine config(A/B/C)の precedence**:

```
builtin 既定 < Store A(host global)< Store C(repo meguri.toml, 読み機構ごとに pin/read)< Store B(host [projects.*] 上書き)
```

host(B)が最後に勝つ原則は不変 — 運用者はいつでもローカルで矯正できる(ADR 0011)。

**Store D は engine config の precedence 鎖に入らない**。D は desired state(入力)であり、
A/B/C は engine config(振る舞い)なので、両者は同じ軸で優先度を争わない。engine は D を
**observe** し、A/B/C の設定に従って D を望ましい状態へ寄せる。「`hold` と
`[reconciler] policy` のどちらが勝つか」のような比較は起きない — 前者は「何をしたいか」、
後者は「どう動くか」で、直交する。

### 4. 混入はエラー、壊れた設定でプロセスを殺さない(不変)

Store C の eligibility は `meguri.toml` の parse gate(`RepoManifest`)の `deny_unknown_fields`
が enforce する(ADR 0011)。A/B 専用キー(`repo_slug` / `[agent]` / `triage.mode` / …)を
`meguri.toml` に書けば parse error、`meguri doctor` が報告する。parse/検証に失敗した
`meguri.toml` は warn + `repo_config.invalid` emit の上で「無いもの扱い」にフォールバック
する(不変)。

## Consequences

### 分類表(partition の総体 — これが「閉じている」の実体)

| store | キー | 読み機構 |
|---|---|---|
| A(host engine) | `mux` `agent(s)` `routing` `drift` `escalation` `launch` `collab` `limits` `scheduler` `daemon` `notifications` `decompose` `reconcile` `reconciler`；`language`/`pr`/`clean`/`triage` の global 既定 | hot reload(process-bound を除く) |
| B(信頼境界) | `[[projects]]` `[[workspaces]]`；`id` `repo_slug` `repo_path` `mode` `deliver` `default_branch` `worktree_root` `autonomy` `review` `plan_delivery`△ `pr.auto_merge`；C 可キーの `[projects.*]` 上書き | hot reload |
| C(repo meguri.toml) | `check_command` `language` `pr.draft` `prompts`★ `worktree_setup`※ `clean`※ `triage.ignore`※ `cadence`※ `notify.labels`※ `schedules` | claim 時 pin(post-fold)/ default-branch 読み |
| D(desired state) | `meguri:plan` `meguri:ready` `meguri:hold` `meguri:needs-human` `meguri:automerge` `meguri:working`/`speccing`/`implementing`(status 軸)；issue 本文 | reconciler observe(ADR 0012) |

★ = 本 slice で物理移設して Store C 可にするキー。※ = 所有 store は C に確定だが読み機構の
実装は staged(読み取り点が run 外 = default-branch 読みの一般化を要する)。△ = 読み取り点が
pre-claim/ambient のため C にせず Store B に留めたキー(読み取り点条件、決定 2 #3)。

- config の地図が 4 store で閉じる。新キーは「どの store の所有か・どの読み機構か」を
  最初に答えるだけでよく、場当たりの `[projects.*]` 増殖が止まる。
- プロジェクト内在の設定が repo と一緒に versioned になる範囲が順次広がる(本 slice は
  `prompts`、以降 staged の `worktree_setup` / `clean` / `cadence` …)。ADR 0011 の狙いへ向かう。
- desired state(D)が config の地図に第一級で載り、「ラベル/本文は engine config を
  上書きするのか?」という混乱が構造的に消える(直交、precedence で争わない)。
- **totality を守る責務**が生まれる。`ProjectConfig` にフィールドを足すたび、その所有
  store を宣言し、C 可なら `meguri.toml` スキーマ側にも足す。これを**テストで機械的に
  守る**(所有の欠落 = ADR 0012 の BEHIND 類の config 版)。

### hot reload 非回帰

Store A/B は従来どおり `ConfigReloader` で hot reload(process-bound の `mux.kind` /
`mux.session` / `[daemon]` / `[collab]` は pin、不変)。Store C は hot reload されず、
**読み機構ごとに pin/read される**(claim 時 pin は run 中不変 = ADR 0011 のセキュリティ芯)。
C 可になったキーも、host `[projects.*]`(B)側の hot reload は従来どおり効く
(B が最後に勝つ)。「repo 化したら host 側の hot 編集が効かなくなる」回帰は起きない。

### 永続 pin 型のバージョン互換(claim pin を Store C 拡張で壊さない)

claim 時 pin は `Checkpoint.repo_config`(= `RepoConfig`)として sqlite に serialize される。
ここには**罠**がある: 現行 `RepoConfig` は `deny_unknown_fields` 付きなので、**新バイナリが
書いた(新キー入りの)checkpoint を旧バイナリが読むと `RepoConfig` の decode が失敗し、
`Checkpoint` 全体が `unwrap_or_default` に落ちて `pr_number` / `base_sha` / `thread_ids` など
既存の pin まで失われる**。#222 / ADR 0026 が「pin 型をバイト安定に保ち schedules を入れない」
ことで守った不変条件は、まさにこれである。

したがって Store C を拡張するときの規律を固定する:

- **eligibility の enforce は `RepoManifest`(parse gate)に置き、永続 pin 型には置かない**。
  `meguri.toml` の混入検出は `RepoManifest` の `deny_unknown_fields` が担うので、永続 pin 型
  (`RepoConfig`)からは `deny_unknown_fields` を**外し**、未知フィールドを**寛容に無視**する。
  こうすれば rollback 先の旧バイナリは知らない pin キーを捨てて残りの `Checkpoint` を保つ
  (= その run はその機能が無かった時の挙動に degrade する。正しい後退)。
- **pin 面は最小に保つ**。あるキーを永続 pin へ足すのは「fold 後の run で読む」かつ「run 中の
  改竄が完了契約上問題になる」ときだけ。run 外読み(clean / cadence / … の default-branch 読み)
  の値は `Checkpoint` に入れない(#222 が schedules を入れなかったのと同じ)。
- 互換テストで両方向を固定する: (新バイナリ ← 旧 checkpoint = 新キー欠落を default で補完) /
  (旧バイナリ ← 新 checkpoint = 未知キーを無視し既存 pin が生存、`unwrap_or_default` に落ちない)。

版番号を切って移行する案もあるが、寛容進化(unknown を無視)の方が単純で schedules の先例にも
沿うので、こちらを採る。

### rejected

- **D を precedence 鎖に入れる**(ラベルと engine config を同じ優先度で比較): desired state と
  engine config を混同する。`hold`(何をしたいか)と `policy`(どう動くか)は直交で、勝ち負けを
  争う関係ではない。
- **section 単位で store を固定する**(`[triage]` 丸ごと host、`[notify]` 丸ごと C 等): spec 軸
  書き込み権限(triage.mode)と誤検知抑制(triage.ignore)は信頼レベルが違う。键単位境界が要る。
- **repo(C)を host(B)より優先させる**: 運用者がローカルで矯正できなくなる(ADR 0011 不変)。
- **`plan_delivery` を C にする**: 読み取り点が pre-claim/ambient に散っており、repo 宣言が
  一部経路にしか届かず経路間で食い違う(決定 2 #3)。読み取り点条件で B に留める。
- **`clean` の分類を再び保留する**: Repo Kind observe という読み取り点がスライス 1〜4 で
  生まれた今、**分類**を閉じない技術的理由は無い(読み機構の実装は staged だが所有 store は確定)。
- **永続 pin 型に `#[serde(default)]` 付きの新フィールドをそのまま足す**: `deny_unknown_fields`
  のため旧バイナリの decode を壊し `Checkpoint` 全体を失わせる(§永続 pin 型の互換)。寛容進化を採る。
- **run 外読みのキーを claim pin(`Checkpoint`)に載せる**: pin 面を肥大させ互換リスクを増やす。
  clean / cadence / notify.labels は default-branch 読みで賄い `Checkpoint` に入れない。
