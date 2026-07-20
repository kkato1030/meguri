# spec: issue #222 — scheduler_fire を Schedule Kind へ（repo-side config を包含）

ADR 0012（level-triggered reconciler）移行のスライス2 / 5。`scheduler_fire` の poll-tick sweep を
**Schedule Kind** の reconciler（observe → 純関数 next_step → act）に載せ替え、あわせて **schedule を
repo-side config（`meguri.toml`）に取り込む**二層化を行う。

durable な設計判断はこの spec には残さない:
- 「schedule を repo-eligible にし、default branch から発見読み取りする」理由 → **ADR 0026**（本 PR で追加）。
- reconciler の Kind 分割・観測境界・enqueue-only の不変 → **ADR 0012 / 0009 / 0015**（既存）。

この spec は使い捨ての足場で、**実装完了時に** spec-worker が削除する（`.claude/rules/docs.md`。実装着手時
ではない — 実装中は本 spec を参照するため）。

## spec 深度: design（深い方）

**理由**: 新しい公開 config 契約（repo `meguri.toml` の `schedules`）を足し、cron が自動で issue/task を
起票するという運用リスクを持ち、かつ受け入れの芯が「kill / resync をまたいだ発火の正しさ（取りこぼしなし。
配信契約は f2 で at-least-once と確定）」という永続状態の正しさである。veto ルール（永続状態・公開契約・
不可逆な運用リスクのいずれかに触れるなら migration & rollback 必須）に該当するため、design 深度で
migration & rollback を書く。

## 受け入れの芯（issue より）

1. cron 起票が **Schedule Kind 経由**で動き、既存の消化ループと非回帰（`tests/schedule_test.rs` が緑）。
2. enqueue-only 原則（ADR 0009）は不変のまま Schedule reconciler 化（pane も run record も作らない）。
3. 発火時刻の永続がローカル store に載り、kill / resync 後も**取りこぼしなし**（no missed fires）。
   配信契約は **at-least-once**（enqueue と state 前進の間で kill されると次 tick が同じ窓を再 enqueue
   しうる）。この crash 由来の重複は **overlap guard では抑えられない**（key 未保存、f6）— 抑えるのは
   enqueue→record 窓の狭さだけで、起きても enqueue-only ゆえ低害・可視。境界は D2 / ADR 0026 に明記（f2/f6）。
4. schedule を repo ルート `meguri.toml` に置け、default branch の内容が効く（ADR 0026）。host 側の
   `[[projects.schedules]]` を書かない既存プロジェクトの挙動は完全に不変。
5. **run を持たない managed clone でも、remote の default branch にマージした schedule が次の sweep で
   発見（observe）される**。effective-set resolver が read の前に `origin/<default_branch>` を fetch するため
   （f1）。発見後は既存契約どおり、初回観測は state を seed し**発火しない**（no-backfill）／その後最初の
   cron 窓で発火する（f4）。
6. **fetch 失敗時は repo schedule 層を abstain する**（fail-closed、f3）。stale ref に戻して発火し続けると
   削除済み schedule を撃ち続けるため、remote があるのに fetch できない tick は repo schedule を読まず・
   撃たず・seed せず、次 tick で再試行する。host schedule はこの ref に依存しないので撃ち続ける。sweep 全体を
   abort もしない。この方針は ADR 0026 に記す。

## 触るファイル

- `src/engine/scheduler_fire.rs` → **`src/engine/schedule.rs` にリネーム**（1 Kind = 1 file、ADR 0012 の
  「Schedule Kind」語彙に揃える）。内部を observe → `next_step`（純関数）→ act に再構成。
- `src/engine/mod.rs` — `pub mod` 名の差し替え、doc comment の参照更新。
- `src/engine/scheduler.rs` — 呼び出し `scheduler_fire::sweep` → `schedule::sweep`（poll tick の位置は
  不変）。
- `src/gitops.rs` —
  - `fetch_default_branch(repo_path, default_branch)` を追加。remote があれば `git fetch origin <branch>` を
    実行し**成否を返す**（`fetch_base_tip` のように stale ref へ黙って落ちない）。`run_git` は timeout 無し
    （`gitops.rs:8`）なので、この fetch は**時間上限を掛ける**（`tokio::time::timeout` で包む等）。上限超過は
    fetch 失敗扱い（D4／f3・f4）。remote が無い repo は fetch 不要＝成功扱い。
  - default branch の tip を **SHA に解決**するヘルパ（`origin/<default>`、無ければ local `<default>`)と、
    その SHA で `meguri.toml` / `body_file` を読む ref 固定読み（既存 `show_file_at_ref` を利用）。config と body を
    同一 snapshot から読むため（D4／f5）。`read_file_at_default_branch` 自体は fetch しないまま据え置く。
- `src/config.rs` —
  - **`RepoConfig`（pin 型・checkpoint 型）は変えない**（`language` / `check_command` / `pr`、
    `deny_unknown_fields`）。`schedules` フィールドを**足さない** — checkpoint の後方互換のため（D3／今回 f1）。
  - **parse envelope `RepoManifest`** を新設。repo-eligible な全 top-level key を知り（`language` /
    `check_command` / `pr` / `schedules`）`deny_unknown_fields` を付ける。`schedules` は寛容な未検証
    フィールド（`Vec<toml::Value>`）。これで host-only key 混入を検出しつつ（今回 f2）、schedule 1行の型エラーで
    envelope を壊さない（前回 f1）。`RepoConfig::load_from_worktree` / default-branch 読みはこの envelope を
    parse し、pin 用 `RepoConfig` を派生する。
  - `Config::validate_schedules` のインラインを**独立2関数** `validate_schedule(mode, &ScheduleConfig)`
    （per-schedule）と `validate_schedule_set_names(&[ScheduleConfig])`（collection 単位の重複 name のみ）に
    切り出す（D7）。**あわせて `validate_workspaces()` の呼び出しを `Config::validate()` 本体へ明示的に移す**
    — 現在この呼び出しは `validate_schedules` 末尾（`config.rs:1724`）が唯一の経路で、リファクタで機械的に
    消すと未定義 project / 二重所属 / workspace ID 重複の検証が丸ごと落ちるため（f4）。
- `src/engine/flow.rs` — `RepoConfig::load_from_worktree` 経由の pin 読み（`flow.rs:485-501`）は envelope から
  派生した `RepoConfig` を pin する（`Checkpoint.repo_config` の型・JSON 形は不変）。
- `src/engine/schedule.rs`（新規、D1）に **effective-set resolver** を置く。sweep / doctor / `meguri schedules`
  の三者が共有する単一経路で、fetch（bounded）→ SHA 固定 → ref 固定 read → `RepoManifest` parse → 検証（D7）→
  host-wins merge（D5）→ 出所と pin SHA 付きの有効集合、を1か所で行う。event は emit せず**診断を data で返す**
  （emit は sweep が edge-triggered に行う、D12）。fetch 失敗 / timeout 時は repo 層 abstain（f3・f4）。
- `src/main.rs` の `doctor_schedules` — resolver を呼び、default branch 上の repo schedules も lint、host/repo
  shadow を報告。**host schedules が空でも早期 return しない**（repo-only プロジェクトを見落とさない、f5）。
- `src/app.rs` の `cmd_schedules` — resolver を呼び、表示を「有効集合（host ∪ repo）＋出所（host/repo）」に
  拡張。**host schedules が空でも早期 return しない**（f5）。
- 参照追随（doc comment / import）: `src/tasks.rs` `src/notify/mod.rs` `src/store/schedules.rs`
  `tests/schedule_test.rs`。
- テスト: `src/engine/schedule.rs` の unit（`next_step` の property test を含む）と、`tests/schedule_test.rs`
  に repo-side schedule のケースを追加。

sqlite migration は**不要**（`schedule_state` のスキーマ・キーは不変）。

## 主要な決定（すべてこの pass で確定）

### D1. モジュール名 = `schedule.rs`
`scheduler_fire.rs` を `schedule.rs` にリネーム。merge_tail スライスの先例（新概念に新ファイル名）に倣い、
ADR 0012 の「Schedule Kind」に語彙を合わせる。`epoch_now` / `schedule_marker` も移動。リネームは機械的で、
挙動は変えない。

### D2. reconciler の型（observe → next_step → act）
merge_tail と同じ三分割にする。schedule の identity は `(project, name)`。

```rust
/// 純粋な観測入力（壁時計・I/O を持ち込まない）。
struct Snapshot {
    seeded: bool,          // schedule_state 行が既にあるか
    due: bool,             // is_due（cron 窓に発生が入るか。既存の純関数を流用）
    allow_overlap: bool,   // 定義値
    last_item_open: bool,  // due かつ !allow_overlap のときだけ意味を持つ
}

/// この Kind が出す Step。ADR 0012 §4 の Op/Wait に対応（agent は起動しない）。
enum Step {
    Seed,               // 初観測: 窓底を seed、発火しない（バックフィル抑止）
    Fire,              // 発火: enqueue + record_schedule_fire(Some(key))
    SkipOverlap,       // due だが直近 item が open: 窓は消費、key は据え置き
    Wait(&'static str), // not due（＝所有 arm が「今は動かない」）
}

fn next_step(s: &Snapshot) -> Step; // 純関数。同じ Snapshot なら常に同じ Step。
```

- `last_item_open` は `due && !allow_overlap` のときだけ forge / store に問い合わせて解決し、それ以外は
  `false` を入れる（`next_step` が `due` を先に見るので無害）。merge_tail が `policy_ok` を遅延解決して
  Snapshot に畳むのと同じ流儀。
- forge が openness を返せない（API エラー）ときは **observe エラーとして per-schedule warn し、次 tick で
  リトライ**（発火も窓前進もしない = 二重発火を避ける）。現行 `fire_one` の `?` 伝播と同じ。Step には
  しない。
- **property test**（ADR 0012 §3）: `(seeded, due, allow_overlap, last_item_open)` の全組合せを列挙し、
  `next_step` が常にちょうど1つの Step を返すこと（所有の欠落も二重所有もない）を機械的に保証する。

**Fire の配信契約（f2、決定 = at-least-once）**。`act` は現行どおり `enqueue`（issue/task 作成）成功後に
`record_schedule_fire` で窓を前進させる。この2手の**間**で kill されると、item は作られたが state は
前の窓のままなので、次 tick が同じ窓を再 enqueue して**重複**する。

- **選択肢 A（exactly-once）** を採らない理由: forge の issue 作成は冪等でなく、厳密な一意化には
  「発火前に (schedule, 窓) の item が既に在るか forge 検索」か「窓を先に前進させてから enqueue」が要る。
  前者は窓を marker に符号化する重い設計で local mode に forge 検索が無く、後者は enqueue 前 crash で
  **取りこぼし**（at-most-once）に反転する。scheduler にとって「取りこぼさない」方が「重複しない」より
  重要なので、この反転は避ける。
- **選択肢 B（at-least-once）を採用**。順序は enqueue → record のまま（＝取りこぼさない）。重複は
  enqueue-only（ADR 0009）ゆえ「余分な issue/task が1件」で人間に可視。
- **overlap guard はこの crash 境界を抑えない（f6、要注意）**。guard は state の `last_key` の open/closed を
  見るが、enqueue→record の crash では作った item の key が `record_schedule_fire` 前で**保存されていない**。
  次 tick の guard が見るのは古い `last_key` なので、guard は crash 由来の重複を防げない。guard が抑えるのは
  「消化が遅く直近 item がまだ open なまま次の cron 窓が来た」通常の重なりだけである。crash 境界の重複を
  抑える唯一の要素は enqueue→record 窓の狭さで、起きても enqueue-only ゆえ低害・可視、というのが正直な線。
- **crash-boundary テスト**を受け入れに追加: enqueue 済み・record 前の state から再 sweep すると同じ窓で
  もう1度発火して**重複が観測される**こと（＝ guard の blind spot を明示。抑止できるとは主張しない）／
  record 済みなら再発火しないこと。

### D3. parse は manifest envelope 1つ、pin 型（checkpoint）は現状不変（ADR 0026 / f1 / f2）
schedule を `meguri.toml` に置けるようにするが、次の3条件を同時に満たす必要がある:

- **(A) schedule 1行の型エラーが完了契約 pin を巻き添えにしない**（前回 f1）。`flow.rs:442` は checkpoint の
  decode 失敗で checkpoint **全体**を default に落とす（`from_str(...).unwrap_or_default()`）。
- **(B) host-only key（`repo_slug` / `agent` …）の混入を検出する**（今回 f2）。`deny_unknown_fields` の無い
  schedule 専用 parser では top-level host-only key を黙殺してしまう。
- **(C) checkpoint に pin する型は旧バイナリと後方互換**（今回 f1）。`Checkpoint.repo_config:
  Option<RepoConfig>` は毎回 serialize され（`flow.rs:351-358, 1043-1045`）、旧 `RepoConfig`
  （`deny_unknown_fields`）が未知 field を拒否すると、上の decode 失敗経路で**保存済み run が丸ごと壊れる**。

これを **manifest envelope** で一挙に満たす:

- **`RepoConfig`（pin 型・checkpoint 型）は現状のまま一切変えない**（`language` / `check_command` /
  `pr`、`deny_unknown_fields`）。**`schedules` フィールドを足さない**。checkpoint の JSON 形は不変なので旧
  バイナリがそのまま decode でき、(C) を満たす。
- 新たに **parse envelope `RepoManifest`** を足す: repo-eligible な全 top-level key を知る型
  （`language` / `check_command` / `pr` / `schedules`）で `deny_unknown_fields` を付ける。
  - `schedules` は**寛容な未検証フィールド**（`#[serde(default)] schedules: Vec<toml::Value>`）にし、
    `[[schedules]]` 1行の型エラーで envelope 全体を壊さない → (A)。
  - `deny_unknown_fields` が repo-eligible key 以外（host-only key）を parse error にする → (B)。正当な
    `check_command` / `language` / `pr` は envelope が知っているので拒否されない（f2 が指摘した「両立しない」を
    envelope が知識を持つことで両立させる）。
- **`RepoConfig::load_from_worktree` / default-branch 読みは、まず `RepoManifest` に parse し、そこから
  pin 用 `RepoConfig`（`language` / `check_command` / `pr` だけ）を派生**する。schedules は raw のまま resolver へ
  渡す。run flow は schedules を読まず、`has_values()` も無視する（schedule だけの `meguri.toml` は run に
  何も畳み込まない）。
- **schedule の型付き化**は resolver 側で、`RepoManifest.schedules` の各 `toml::Value` を `ScheduleConfig` へ
  `try_into` して行う（1要素の失敗 = per-schedule エラー = その1件 drop、D6）。

回帰テスト（f1/f2/checkpoint）:
- `[[schedules]]` が壊れた `meguri.toml` でも pin 用 `RepoConfig` は valid で `check_command` が生き残る (A)。
- top-level host-only key（例 `repo_slug`）は envelope parse error として検出される (B)。
- **新バイナリが書いた checkpoint（`repo_config` に schedules フィールド無し）を旧 `RepoConfig` decoder が
  読める**ことを固定する rollback テスト (C)。

### D4. 有効 schedule 集合 = host ∪ repo（default branch）を単一 resolver で
発火対象は host `deps.project.schedules` と、default branch の `meguri.toml` の schedules の**和**として作る。
claim 時 pin ではなく発見読み取り（ADR 0015 / 0026）。この解決は sweep / doctor / `meguri schedules` が
共有する**単一の effective-set resolver**（`engine::schedule`）に閉じ込める（f5）。resolver の手順:

1. **fetch（bounded）**: remote があれば `gitops::fetch_default_branch` で `origin/<default_branch>` を更新。
   ただし `run_git` は timeout を持たない（`gitops.rs:8`）ので、この fetch は**必ず時間上限を掛ける**
   （下の f4）。
2. **pin SHA**: `origin/<default_branch>`（remote 無しは local `<default>`）を**1回だけ SHA に解決**する。
   以後の read はこの SHA に固定し、可変ブランチ ref を都度読まない（下の f5）。
3. **read**: その SHA で `meguri.toml` を読む（`gitops::show_file_at_ref` 等の ref 固定読み）。
4. **parse**: `RepoManifest`（D3、`deny_unknown_fields` + 寛容 schedules）で parse。host-only key 混入は
   ここで parse error（collection エラー）。schedules の各要素を `ScheduleConfig` へ `try_into`。
5. **validate**: D7 の2検証（collection エラーは集合ごと drop、per-schedule エラーは1件 drop）。
6. **merge**: host-wins で host 集合に重ねる（D5）。各エントリに**出所（host/repo）と pin SHA** を付けて返す。

**freshness と fetch 失敗時の方針（f1 / f3）**。`read_file_at_default_branch` は fetch せず
`origin/<default_branch>` を読むだけで「run flow が ref を fetch 済み」を前提にしている。schedule discovery は
run を持たないのでこの前提が届かず、managed clone では ref が古いまま新 schedule を見落とし／削除済み
schedule を撃ち続ける。そこで resolver が step 1 で fetch する。**fetch が失敗したら repo schedule 層を
abstain する**（fail-closed）: stale ref から読むと、削除された schedule を撃ち続けるし、stale な定義で撃つ
ことにもなるため、その tick は repo schedules を読まず・撃たず・seed せず、次 tick で再試行する。host
schedules はこの ref に依存しないので撃ち続け、sweep 全体も abort しない。remote が無い repo は staleness が
起きないので fetch を要さず local `<default>` を authoritative に読む。

> なぜ best-effort（stale フォールバック）でも fail-hard（sweep 全体 abort）でもないか: `fetch_base_tip` の
> best-effort は stale ref を許し「削除 schedule を撃ち続ける」誤りを生む。`fetch_branch_tip` の fail-hard は
> 不可逆な1件の起票を gate する用途で、そこでは正しいが、schedule sweep 全体（host 分含む）を巻き込むのは
> 過剰。よって「repo 層だけ fail-closed に abstain、host 層と sweep は継続」を採る。両方向（追加・削除）の
> 帰結を踏まえた決定として ADR 0026 に記す。

**fetch の hang が host schedule を止めない（f4）**。`run_git` は timeout 無し（`gitops.rs:8`）なので、認証
helper や SSH が固まると fetch の await が返らず、直列 scheduler ではその project の host schedules と後続
project まで止まる。「fetch 失敗時も host は撃つ」を実際に成立させるため:

- fetch には**必ず時間上限を掛ける**（`tokio::time::timeout` で `fetch_default_branch` を包む、または git 側に
  `--` timeout を渡す）。上限超過は **fetch 失敗として扱い repo 層 abstain**（上の f3）へ落とす。これで hang が
  無限待ちでなく有界の abstain になる。
- **host schedule の発火は repo 層の fetch 結果に依存させない**。host の定義は in-memory config なので I/O 不要。
  resolver は repo 層が abstain（fetch 失敗 / timeout）でも host 集合を返し、sweep は host を撃つ。fetch の
  成否が host 発火の前提条件にならないよう境界を引く。

**config と body_file を同一 commit から読む（f5）**。resolver は可変な `origin/<default>` から `meguri.toml` を
読み、Fire 時の `render_body` は後で同じ ref を**再解決**して `body_file` を読む（現
`scheduler_fire.rs:202-212`）。並行 fetch で ref が動くと、定義は commit A・本文は commit B になり、B で
body_file が消えていれば発火を落とす。これを塞ぐため、resolver が step 2 で固定した **pin SHA を fire まで
持ち回り**、`render_body` も**その SHA で** `body_file` を読む（`show_file_at_ref`）。定義と本文は必ず同一
snapshot から読まれる。統合テストで「read 後・fire 前に remote を進めても、fire は元 SHA の body_file を使う」
ことを確認する。

### D5. 名前衝突は host が勝つ
同名 schedule は host 定義を採用し、repo 側は落とす。`schedule.shadowed`（`{project, name}`）を emit + warn。
黙殺しない。

### D6. 壊れた repo config はプロセスを殺さない（検証エラーの2階層）
検証エラーを **collection 単位** と **per-schedule 単位** に分けて扱う（f3）。

- **collection 単位**（同一 repo 内の重複 `name`、TOML parse 失敗、host-only キー混入）: repo 由来
  schedule 集合を**丸ごと**「無いもの扱い」にフォールバック（warn + `repo_config.invalid` emit）。host の
  重複拒否が collection 単位の hard-fail なのに合わせ、repo 側も同名2件を黙って片方落とさない
  （同じ state key での二重処理も、silent drop も避ける）。ADR 0011「混入は静かに無視せずエラー」に沿う。
- **per-schedule 単位**（cron 不正 / body と body_file の排他違反 / body_file パス不正 / local×plan）:
  その1件だけ落として残りは活かす（sweep の per-schedule 失敗隔離と同じ）。

いずれも host schedule はそのまま発火し、プロセスは殺さない（ADR 0011「壊れた設定でプロセスを殺さない」）。

### D7. 検証ロジックの一本化（2つの検証は別レイヤ・別戻り値）
`Config::validate_schedules` にインラインだった検証を **2つの独立関数**に切り出す。D6 の「collection ごと
drop / 1件だけ drop」を caller が区別できるよう、両者は**混ぜない**（f5）:

- `validate_schedule(mode, &ScheduleConfig) -> Result<()>` — per-schedule ルールだけ（cron parse /
  body xor body_file / body_file の repo-relative 安全性 / local mode × `kind=plan` 拒否）。要素の検査に
  専念し、重複 name は見ない。
- `validate_schedule_set_names(&[ScheduleConfig]) -> Result<()>` — collection 単位の重複 `name` 検査**だけ**。
  per-schedule 検証は**含めない**。

caller はこの2つを**別々に**呼び分ける（単一 `Result` で2種のエラーを混ぜないので、どちらの disposition か
一意に決まる）:

- **host（config load）**: `validate_schedule_set_names` と各要素の `validate_schedule` を両方呼び、どちらの
  エラーも hard-fail（既存挙動そのまま）。
- **repo（resolver: sweep / doctor / `meguri schedules`）**: まず `validate_schedule_set_names` を呼び、
  Err なら **repo 集合ごと drop**（collection エラー、D6）。通れば各要素に `validate_schedule` を適用し、
  **Err の要素だけ drop** して残りを活かす（per-schedule エラー、D6）。

host と repo の **cross-layer** 同名衝突は、こうして得た repo 有効集合を host 集合に重ねるときに D5 で解決する
（host 勝ち + `schedule.shadowed`）。つまり同名衝突は「同一 repo 内 = `validate_schedule_set_names` の
collection エラーで repo 集合 drop（D6）」「host×repo 間 = host 勝ち（D5）」の2経路で漏れなく閉じる。

**workspace 検証を落とさない（f4）**。現行 `Config::validate_schedules` は末尾で `self.validate_workspaces()?`
を呼び、それが唯一の呼び出し（`config.rs:1724`）である。上のように validator を切り出すと、この呼び出しを
機械的に消して未定義 project / 二重所属 / workspace ID 重複が素通りしうる。これを防ぐため、
`validate_workspaces()` の呼び出しを `Config::validate()` 本体（per-project ループの外、1回だけ）へ**明示的に
移す**。既存の workspace テストを非回帰対象に含める。

### D8. doctor / `meguri schedules` は runtime と同じ有効集合を見る（f5）
`doctor_schedules`（`src/main.rs`）と `cmd_schedules`（`src/app.rs`）は、D4 の **effective-set resolver を
そのまま呼ぶ**。これで表示と実発火が同じ fetch・parse・検証・host-wins merge を経た同一集合を見る。

- **早期終了を外す**。現行はどちらも host schedules が空だと早期 return / continue する
  （`main.rs:500-510` / `app.rs:1535-1537`）ため、schedules を repo にだけ置いた**repo-only プロジェクトを
  丸ごと見落とす**。resolver 経由に変え、この early return を撤去する。
- doctor は resolver の出所（host/repo）と cron / body_file の妥当性、host×repo shadow を報告する。fetch 失敗で
  repo 層を abstain した場合はその旨を出す（stale を黙って host-only 表示にしない）。doctor が repo 側検証の
  人間向け面という ADR 0015 の役割は不変。
- `meguri schedules` は有効集合を出所列付きで表示する。repo schedule が黙って発火して CLI に見えない観測
  ギャップを塞ぐ。

### D10. 発火状態のキーは不変
`schedule_state` は `(project_id, name)` キーのまま。host ↔ repo の移動は name が同じなら state を継ぐ
（取りこぼし・再バックフィルなし。重複は f2 の at-least-once 境界に従う）。sqlite migration なし。

### D11. enqueue-only は不変（ADR 0009）
発火は issue（github）/ task（local）を1件作るだけ。pane も run record も作らない。reconciler 化で
ここは一切変えない。

### D12. 定常状態の診断は毎 tick 書かない（edge-triggered、f6）
`schedule.shadowed` / `repo_config.invalid` / `schedule.repo_unavailable` は、条件が続く限り resolver ごと
（＝ poll ごと）に成立する。`Store::emit` は無条件 INSERT（`events.rs:29-36`）なので、そのまま emit すると
同じ診断が tick ごとに積もる。そこで:

- **resolver は event を emit しない**。有効集合と一緒に**診断を data として返す**（純粋な出力）。これで
  doctor / `meguri schedules` から resolver を呼んでも副作用が無い。
- **emit するのは sweep だけ、しかも transition 時のみ**（edge-triggered）。sweep は project ごとに
  「直近 tick の診断 digest」を**プロセス内メモリ**に持ち（`open_prs` キャッシュと同種、schema 追加なし）、
  診断が**新規発生・内容変化・解消**したときだけ event を出す。定常状態の tick は event を書かない
  （必要なら `tracing::debug!` に留める）。event ログは「状態が続いている」ではなく「状態が変わった」の記録に
  なる。
- restart 後はメモリが空なので各診断を1回だけ出し直す（許容 — 高々1回）。

これは ADR 0020（notify-sink は event-driven / best-effort）の精神と揃う。durable な粒度の話ではなく実装契約
なので spec に置く。

- **影響**: scheduler_fire は既に out-of-band sweep で state を sqlite に持ち、`is_due` は純関数で、
  enqueue-only。したがって本スライスの主眼は**語彙の再構成**（Snapshot / next_step / Step / property test）と
  **二層化**であり、消化ループ（worker / planner）の discover 経路には触れない。poll tick 内の呼び出し位置も
  不変。
- **二層化の芯は "隔離" と "単一 resolver"**: schedule の parse を run flow の pin（`RepoConfig`、寛容な
  未検証フィールド）から型レベルで切り離し（D3 / f1）、有効集合の解決を fetch・parse・検証・merge を束ねる
  単一 resolver に集約して sweep / doctor / CLI が同じ集合を見る（D4 / D8 / f5）。この2点が、レビューで露呈した
  「schedule エラーが完了契約を巻き添え」「表示と発火のズレ」を構造的に閉じる。
- **代替案（schedule 読み取りの出所）**: worktree から読む案／claim pin する案は ADR 0026「却下した代替案」
  参照（run が無い・working tree 依存・bare clone で壊れる、で却下）。
- **代替案（reconciler を作らず現状維持）**: ADR 0012 のスライス移行で「全 Kind が reconciler 経由」を
  成立させるため、Schedule Kind を残す選択肢は無い（スライス4 で旧 `Loop` trait を撤去する前提）。

## migration & rollback

- **データ移行**: なし。`schedule_state` のスキーマ・キー・意味は不変。既存プロジェクトは host config の
  `[[projects.schedules]]` のまま動き、state もそのまま引き継ぐ。
- **前方移行（host → repo への移設手順）**: 運用者が schedule を repo 化するときは、host の
  `[[projects.schedules]]` から同名定義を `meguri.toml` に移して default branch にマージする。**name を
  保てば** state が継続し、切替時に取りこぼしは起きない（D10。at-least-once ゆえ移設タイミングと kill が
  重なると重複しうるが、それは f2 の契約どおり — この crash 重複は overlap guard では抑えられず、窓の狭さと
  enqueue-only の低害性で受容する、f6）。過渡的に host と repo に同名が並んでも host が勝つ（D5）ので、
  二重登録による定常の重複は起きない。
- **rollback（順序が要る、f2）**: コード面の rollback は PR revert だが、**revert だけでは完了しない**。
  `[[schedules]]` が default branch の `meguri.toml` に残ったまま #222 前のコードへ戻すと、旧 `RepoConfig` は
  `deny_unknown_fields` により `schedules` を未知キーとして弾き、**ファイル全体の parse が失敗**する。すると
  各 run は `RepoConfig::default()` に落ち、同じファイルの `check_command` / `language` / `pr.draft` まで失う
  （D3 と同じ経路）。したがって rollback は次の順序を守る:
  1. **先に** default branch の `meguri.toml` から `[[schedules]]` を除去してマージする（発見読み取りが
     空になり host 定義だけに戻る。sqlite state は残るので host に同名で戻せば発火履歴も継続）。
  2. その後にコードを revert する。
  この順序なら旧コードが弾く `[[schedules]]` は既に無く、pin は健全なまま戻る。#222 前のコードは
  変更できない以上、後方互換 parse では塞げず、**順序で担保する**のが唯一の手段である。migration ドキュメント
  （PR 説明）にこの順序を明記する。
  - **in-flight checkpoint は安全（今回 f1）**: pin 型 `RepoConfig` を D3 で**一切変えない**（`schedules`
    フィールドを足さない）ので、新バイナリが保存した `Checkpoint.repo_config` の JSON 形は旧バイナリと同じ。
    旧 decoder が新 checkpoint を読めるため、上の順序を守れば保存済み run も壊れない。これを rollback テストで
    固定する（D3）。もし pin 型に schedules を足していたら、repo ファイルを消しても保存済み checkpoint が
    未知 field を持ち、`flow.rs:442` の `unwrap_or_default()` で checkpoint 全体を失っていた — それを避けた。
- **不可逆リスクの評価**: 最悪ケースは「repo に誤った cron を書いて過剰起票する」だが、(a) enqueue-only で
  やることは issue 作成のみ、(b) overlap guard が直近 item が open の間の**通常の重なり**を skip（crash 由来の
  重複は抑えないが低害・可視、f6）、(c) 反映には default branch への commit = 人間マージゲート /
  branch protection が要る、で緩和される。

## observability

- 既存イベントは不変: `schedule.fired` / `schedule.skipped` / `schedule.failed`。
- 追加イベント（すべて **edge-triggered**。定常状態では毎 tick 書かず、発生・変化・解消の遷移でだけ出す。
  D12 / f6）:
  - `schedule.shadowed`（`{project, name}`）— host×repo の同名で repo 側が host に負けて落ちたとき（D5）。
  - `repo_config.invalid` — default branch の `meguri.toml` が parse / collection 検証（重複 name 等）に
    失敗し repo schedule 集合を無効化したとき（D6、既存イベント名の再利用）。
  - `schedule.repo_unavailable`（`{project}`）— fetch 失敗 / timeout で repo schedule 層を abstain した状態
    （f3 / f4）。stale を黙って握り潰さず、なぜ repo schedule が撃たれなかったかを追えるようにする。
- `schedule.fired` の payload に出所（host/repo）を足すと、どの層由来の発火かを後から追える。
- `meguri schedules` / `meguri doctor` が有効集合と shadow を表示（D8。表示は resolver を呼ぶだけで event は
  emit しない、D12）。

## test strategy

- **unit（`src/engine/schedule.rs`）**:
  - `is_due` の既存テスト（窓 / catch-up 折り畳み / no-backfill）を維持。
  - `next_step` の property test（D2）: 全組合せで単一 Step を保証。
  - `Seed` / `Fire` / `SkipOverlap` / `Wait` の分岐ごとの単体。
  - **crash-boundary（f2 / f6）**: enqueue 済み・record 前の state から再 sweep すると同じ窓で**もう1度
    発火し重複が観測される**こと（overlap guard は抑えられない blind spot の明示）／record 済みなら再発火
    しないこと。
- **config（`src/config.rs`）**:
  - **pin 隔離（回帰）**: `[[schedules]]` の中身が壊れた（`title` 欠落など）`meguri.toml` でも、envelope から
    派生する pin 用 `RepoConfig` は valid で `check_command` / `language` / `pr.draft` が生き残ること。schedule の
    型エラーは resolver の `try_into` 側でだけ現れること。
  - **host-only key 検出（今回 f2）**: envelope `RepoManifest` は `repo_slug` 等 top-level host-only key を
    `deny_unknown_fields` で parse error にすること。正当な `check_command` / `language` / `pr` は通ること。
  - **checkpoint 後方互換（今回 f1）**: 新コードが書いた `Checkpoint`（`repo_config` に schedules field 無し）の
    JSON を、schedules を知らない旧 `RepoConfig` decoder（`deny_unknown_fields`）が `unwrap_or_default` に
    落ちず decode できること。pin 型が不変であることの固定。
  - `validate_schedule`（per-schedule）と `validate_schedule_set_names`（重複 name のみ）が別々に呼べ、
    混ざらないこと。**host 内・repo 内の重複 name**: host は load 時 hard-fail、repo は集合ごと drop。
    **per-schedule エラー1件は1件だけ drop**（repo で cron 不正1件を混ぜ残りは活きる）。
  - **workspace 検証の非回帰（前回 f4）**: `validate_workspaces` の呼び出しを `Config::validate()` に移した後も、
    未定義 project / 二重所属 / workspace ID 重複が load 時に hard-fail する既存テストが緑であること。
- **統合（`tests/schedule_test.rs`）**:
  - 既存ケース（発火 / catch-up / backfill 抑止 / overlap guard / hot-reload 追加）を `schedule::sweep`
    経由で非回帰（芯1）。
  - repo-side schedule: 実 git worktree の default branch に `meguri.toml` を commit → 発見・seed 後、
    最初の cron 窓（clock を進める）で発火すること（芯4）。
  - **stale clone freshness（f1 / f4、芯5）**: 古い clone を用意し、remote の default branch を schedule 入り
    `meguri.toml` で更新する。手順は no-backfill 契約に整合させる（f4）:
    1. sweep-1 が（前段 fetch を経て）新 schedule を**発見・seed**し、まだ**発火しない**こと。
    2. injected clock を最初の cron 窓の先へ進めた sweep-2 で**発火**すること。
    freshness 単体だけを見たい場合は、事前に同名 state を seed してから remote 更新 → 1 回の sweep で
    発火、と縮めてもよい（発見経路が生きていることの最小確認）。
  - **削除方向の abstain（f3）**: repo schedule を1件持つ状態から、fetch を失敗させた tick では repo schedule を
    **撃たない**（削除済みを stale に撃たない・stale 定義で撃たない）こと、`schedule.repo_unavailable` が出て
    host schedule は撃たれること。fetch 回復後の tick で正しい repo 集合に追随すること。
  - host/repo 同名: host が勝ち `schedule.shadowed` が出ること（D5）。
  - 壊れた / 重複 name の `meguri.toml`: repo schedule 集合は無効化されるが host schedule は発火すること
    （D6 / f3）。cron 不正が1件だけの repo `meguri.toml` では、その1件だけ落ちて残りは発火すること（f5）。
  - **repo-only プロジェクト（前回 f5）**: host schedules が空でも repo schedule が resolver 経由で発火し、
    `meguri schedules` / doctor が repo-only の有効集合を表示すること（早期 return を外した確認）。
  - **同一 commit snapshot（今回 f5）**: resolver が `meguri.toml` を読んだ後、fire 前に remote の default branch を
    進めて `body_file` を消しても、fire は resolver が固定した pin SHA の `body_file` を使って発火すること
    （定義と本文が別 commit に割れない）。
  - **fetch hang で host が止まらない（今回 f4）**: fetch を（timeout で）失敗させた tick で、その project の
    host schedule と後続 project が待たされずに撃たれること／`schedule.repo_unavailable` が出ること。
  - **診断は edge-triggered（今回 f6）**: 同じ shadow / invalid / repo_unavailable 条件が続く複数 tick で
    event が**1回だけ**（遷移時）emit され毎 tick 増えないこと。解消時にも1回出ること。
  - host ↔ repo 移設で name 一致なら取りこぼさないこと（芯3 / D10）。
