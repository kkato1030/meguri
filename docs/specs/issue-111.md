# issue-111 spec — 実装中の worker が plan 作者に agmsg で相談できる助言レイヤ(collab-advisor)

routing(#64)は「どの役割をどのモデルに振るか」を決めた。この spec の決定は一行で書ける。**振り分けた役割モデル同士を実行中に喋らせる** — worker の execute 中に、plan 作者の役割(planner)を助言者(advisor)として同じ issue に再具現し、worker が [agmsg](https://github.com/fujibee/agmsg) 越しに「要件を満たしているか / ブレていないか」を相談できる助言レイヤを、オプトインで足す。

設計の三原則は **ADR 0006**(本 PR 同梱)に置いた。この spec はその実装形と受け入れ基準に絞る。要約:

1. **advisor = 役割の再具現**。死んだ planner session の復活ではなく、planner プロファイルで新しい pane を issue に立て、merge 済み spec で seed する。第 3 lane(advisor)を意図的に足す(ADR 0004 の「必要になったら再訪」の再訪)。
2. **相談は完了契約の外**。meguri は agmsg を読まない・待たない・検証しない。run の成否は `result.json` + git 検証だけで決まる。
3. **advisor は ephemeral**。worker execute で spawn、worker run 終了で必ず reap、`keep_pane` に依存しない。

## 二つの「役割」を混同しない

この機能には別種の「役割」が二つ出てくる。spec 全体でこの区別を守る。

- **pane lane**(`src/store/panes.rs` の `role` 列):`author` / `review` の 2 値。ここに **`advisor` を第 3 の lane として足す**。pane の鍵は `(project, issue, lane)`(#92)。
- **routing role / loop_kind**(profile 選択の軸、`src/routing.rs`):`planner` / `worker` / … 。advisor は**新しい routing role を足さず**、`planner` のプロファイル解決をそのまま借りる。だから routing 表(ADR 0003)は無変更で「plan を作ったモデル」が助言に立つ。

## 決定と実装形

### 1. config `[collab]` セクション — `src/config.rs`

routing の「セクションの有無がスイッチ」規律(ADR 0003)に倣う。`RoutingConfig` と同型の小さな `Option` フィールドを `Config` に足す(`src/config.rs:78` `routing` の隣)。

```toml
[collab]
mode = "advisor"          # "off"(既定)| "advisor"
advisor_role = "planner"  # advisor が借りる routing role(既定 "planner")
# transport は agmsg 固定(v1)。command 名も "agmsg" 固定。
```

- `collab: Option<CollabConfig>`。**セクション無し = 機能 off**(現状とバイト単位で同一)。
- `CollabConfig { mode: CollabMode, advisor_role: String }`。`CollabMode = Off | Advisor`(serde lowercase、既定 `Off`)。`advisor_role` 既定 `"planner"`。
- `mode = "off"` は明示 off(セクションはあるが不活性)。**有効化は `mode = "advisor"` の一点**。
- watch の毎 tick 再読込(#73/ConfigReloader)に自動で乗る(process-bound ではないので pin 不要)。

### 2. 起動時の agmsg 検出 — `src/collab.rs`(新規)+ 起動時 validate

routing の CLI 検出(`routing::detect_command` = `command --version` が exit 0)を**流用**する。routing の logic が `src/routing.rs` に、config 型が `src/config.rs` にあるのと同じ分業で、collab の runtime logic を **`src/collab.rs`** に置く:

- `pub fn validate(cfg: &Config, detect: &dyn Fn(&str) -> bool) -> Result<()>`:`mode = "advisor"` のとき `agmsg` が検出できなければ `bail!`(routing::validate と同じ「起動時に大きな音を立てて落ちる」流儀)。`advisor_role` が未知 routing role なら同様に startup error。`mode = "off"` / セクション無しは no-op。
- `routing::validate` を呼んでいる起動経路(`meguri watch` / `meguri run` の入口)から `collab::validate` も呼ぶ。**silent fallback しない**。
- `pub fn team_name(project_id, issue) -> String`:agmsg の共有 SQLite floor は複数 project を横断しうるので、team は `meguri-<project_id>-<issue>` で衝突を避ける(issue の「team = issue 単位」を project でスコープ)。
- prompt 断片ビルダ 2 つ(下記 4)。

### 3. advisor の spawn / reap — `src/engine/flow.rs`

**新 lane 定数** `ROLE_ADVISOR = "advisor"` を `src/store/panes.rs` に足す。

**Flavor に opt-in の口を足す**(`src/engine/flow.rs` の `Flavor` trait):

```rust
/// この loop が collab-advisor の対象か(既定 false)。worker / spec-worker が true。
fn supports_advisor(&self) -> bool { false }
```

`WorkerFlavor`(`src/engine/worker.rs`)と `SpecWorkerFlavor`(`src/engine/spec_worker.rs`)だけ `true` を返す。

**spawn(execute 前)**:`drive` の `STEP_EXECUTE` ブロック(`flow.rs:339` 付近、worktree 再読込の直後・`execute(...)` の直前)で、`config.collab` が有効 かつ `flavor.supports_advisor()` のとき `ensure_advisor(deps, run, cp)` を呼ぶ。

- advisor pane は `ensure_pane` と同型で `(project, issue, ROLE_ADVISOR)` を鍵に冪等に確保(resume 復帰時に二重に立てない。既存 live pane は adopt)。
- profile は `routing::resolve(cfg, cfg.collab.advisor_role, detect)`(既定 `"planner"`)。**worker の profile ピン(`runs.agent_profile`)とは無関係**に解決する。
- cwd は **review lane と同じく base の detached read-only checkout**(worker の worktree を共有しない = 二重書き込みを避ける)。seed は spec 全文をプロンプトに inline(spec-worker が spec を読み込むのと同型、`spec_worker.rs:194-235`)。spec ファイルが無い `meguri:ready` 直行 issue では issue 本文を seed にフォールバック。
- **best-effort**:advisor spawn が失敗しても run は止めず、`collab.advisor_spawn_failed` を emit して worker は advisor 無しで進む(ADR 0006 原則 2)。

**reap(run 終了)**:`run_flow` の終端 match(`flow.rs:257-308` の全 arm:成功 / needs-plan / decompose / 中断 / 失敗)で `release_advisor(deps, &run)` を呼ぶ。

- 中身は `reaper::release_pane(deps, issue, ROLE_ADVISOR, "worker run ended")`。`keep_pane` を見ない(常に reap。ADR 0006 原則 3)。
- collab 無効 or advisor pane 不在なら no-op(冪等・安全)。

**reaper 安全網** — `src/engine/reaper.rs`:lane 走査(`plan_panes` / worktree-alive guard、現状 `[ROLE_AUTHOR, ROLE_REVIEW]`)に `ROLE_ADVISOR` を足し、万一 run 終端の reap が漏れても issue close 掃引で回収されるようにする。正常経路では run 終端で先に消えている。

### 4. プロトコルはプロンプトに置く — worker 相談ブロック + advisor seed

agmsg 自身が「トランスポートは素朴に、プロトコルは各 agent のプロンプトに委ねる」設計(ADR 0006 の文脈)。meguri は agmsg を exec しない。両者に team 名と相手の agmsg id を渡すだけで、あとは agent が agmsg を叩く。

- **addressing**:team = `meguri-<project>-<issue>`、advisor の agmsg id = `advisor`、worker の id = `worker`。両プロンプトに team と相手 id を埋める。
- **worker execute プロンプトへの追記**(`src/engine/worker.rs:57` `execute_prompt`、collab 有効時のみ append。無効時は**バイト単位で不変** = 受け入れ基準 1):

  > **相談してよい相手がいる。** この issue の spec を書いた助言者が agmsg team `meguri-<project>-<issue>` に id `advisor` で待機している。実装の方針が spec の要件を満たしているか / ブレていないか迷ったら、agmsg で `advisor` に相談してよい(相手は spec 全文を見ている)。相談は任意で、完了条件ではない。コードのレビューではなく要件充足とドリフトの相談に使え。

  spec-worker(`spec_worker.rs:194-235`)にも同じブロックを collab 有効時に append。
- **advisor seed プロンプト**(`src/collab.rs` のビルダ、planner プロファイルで起動する pane の初期プロンプト):

  > お前は GitHub issue #N の spec を書いた本人(planner)だ。以下がその spec 全文だ。\n\n<spec 全文 or issue 本文>\n\n 実装中の worker が agmsg team `meguri-<project>-<issue>` から id `advisor` 宛に相談してくる。**要件充足とドリフトの観点だけ**で簡潔に答えろ。コードは書くな・コミットするな・ファイルを変更するな。worker が要件から外れていたら spec のどの部分に照らしてズレているかを指摘しろ。相談が来るまで待て。

- worker / advisor が agmsg をどう待ち受け・ポーリングするかは**プロンプトに委ねる**(v1 の割り切り)。meguri は関与しない。

## 変わらないもの(意図どおり)

- **完了契約は不変。** `result.json` + git 検証(clean tree / commits ahead / check pass)だけで成否が決まる。agmsg のやり取りの有無・内容は run に影響しない。
- **routing 表は無変更。** advisor は `planner` role のプロファイルを借りるだけ。`KNOWN_ROLES` / `recommended_chain` に手を入れない。
- **author / review lane の寿命は無変更。** `keep_pane` の規律はそのまま。advisor だけが run 終端 reap の別規律。
- **impl-reviewer(#84)は無変更。** collab-consult(実装前・同期・worker 発・助言のみ)と impl-reviewer(PR 後・非同期・meguri 発・durable な review スレッド)は畳まず補完。
- **セクション無し = 現状と完全同一。** advisor を spawn せず、プロンプトはバイト単位で不変。

## 受け入れ基準(acceptance criteria)

1. `[collab]` 無し(or `mode = "off"`)→ 現状とバイト単位で同一:advisor を spawn せず、worker / spec-worker の execute プロンプトが不変(テストで文字列一致を担保)。
2. `[collab] mode = "advisor"` かつ agmsg 検出あり → worker run で `advisor` lane に planner プロファイルの pane が `(project, issue, advisor)` に立ち、worker プロンプトに team 名と相談先 id `advisor` が入る。
3. `mode = "advisor"` + agmsg 未検出 → `meguri watch` / `meguri run` が起動時に明示エラーで停止(silent fallback しない)。`collab::validate` の単体テストで担保。
4. 完了契約は不変:run 時の advisor spawn が失敗しても worker run は止まらず(best-effort)、成否は `result.json` + git 検証だけで決まる(テストで担保)。
5. advisor は worker run の終了(成功 / needs-plan / decompose / 中断 / 失敗のいずれでも)で確実に reap され、`keep_pane = "until-issue-closed"` でも常駐しない。
6. 対象は worker / spec-worker のみ。他 loop(planner / reviewer / fixer / …)は collab 有効でも advisor を spawn しない(`supports_advisor()` = false)。
7. README(en/ja)が `[collab]` 節と「routing の次段」の位置づけを記述している。

## テスト計画

- `src/collab.rs` の単体テスト:`validate`(off / advisor×検出あり / advisor×未検出→bail / 未知 advisor_role→bail)、`team_name`、プロンプト断片ビルダ。routing のテストが detector を closure 注入するパターン(`routing.rs:247` `only(...)`)をそのまま流用しサブプロセスを起こさない。
- `src/config.rs`:`[collab]` の parse(既定 off / mode / advisor_role)、セクション無しで `collab.is_none()`。
- `worker.rs` / `spec_worker.rs`:execute プロンプトが collab off で不変(基準 1)、on で相談ブロックを含む(基準 2)。既存の `prompt_invites_needs_plan` パターン(`worker.rs:196`)に倣う。
- flow 統合(FakeMux + FakeForge):collab on で advisor pane が spawn され run 終端で release される(基準 5)、advisor spawn 失敗を注入しても run が成功する(基準 4)、collab off で pane 数不変(基準 1)。`supports_advisor()` が false の loop で spawn されない(基準 6)。

## 触るファイル

- `src/config.rs` — `[collab]` セクション(`CollabConfig` / `CollabMode`、`Config.collab: Option<_>`)
- `src/collab.rs` — 新規。`validate`(agmsg 検出 = routing 流用)、`team_name`、seed / 相談ブロックのプロンプトビルダ
- `src/store/panes.rs` — `ROLE_ADVISOR = "advisor"` 定数
- `src/engine/flow.rs` — `Flavor::supports_advisor`、`ensure_advisor`(execute 前 spawn、best-effort)、`release_advisor`(run 終端 reap)
- `src/engine/worker.rs` / `src/engine/spec_worker.rs` — `supports_advisor() = true`、execute プロンプトへの相談ブロック append(collab 有効時のみ)
- `src/engine/reaper.rs` — lane 走査に `ROLE_ADVISOR` を安全網として追加
- 起動経路(`routing::validate` 呼び出し元)— `collab::validate` を併せて呼ぶ
- `README.md` / `README.ja.md` — `[collab]` 節(routing 節の後ろ、`README.md:279` 付近)
- `docs/adr/0006-collab-advisor-role-reembodiment.md` — 決定の記録(本 PR に同梱済み)
- テスト:`src/collab.rs` 単体 + flow 統合テスト(既存 `tests/` の FakeMux/FakeForge パターン)

## スコープ外(将来の話)

- 対象 loop の拡大(worker / spec-worker 以外)、多者 team、advisor 常駐。
- meguri による agmsg history からの stall 検知、monitor/turn の delivery mode 調整(meguri は依然 agmsg を読まない前提を崩す変更)。
- transport の抽象化(agmsg 以外)、`agmsg` command 名の config 化。
- agmsg のポーリング / 待ち受けエルゴノミクスの meguri 側最適化(v1 はプロンプト任せ)。
