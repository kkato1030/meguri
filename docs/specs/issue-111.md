# issue-111 spec — 実装中の worker が plan 作者に agmsg で相談できる助言レイヤ(collab-advisor)

routing(#64)は「どの役割をどのモデルに振るか」を決めた。この spec の決定は一行で書ける。**振り分けた役割モデル同士を実行中に喋らせる** — worker の execute 中に、plan 作者の役割(planner)を助言者(advisor)として同じ issue に再具現し、worker が [agmsg](https://github.com/fujibee/agmsg) 越しに「要件を満たしているか / ブレていないか」を相談できる助言レイヤを、オプトインで足す。

設計の三原則は **ADR 0006**(本 PR 同梱)に置いた。この spec はその実装形と受け入れ基準に絞る。要約:

1. **advisor = 役割の再具現**。既定運用では planner の session は author lane として生きて worker に継がれている(同一 session に advisor を兼ねさせると自問自答になる)。その carryover の延命・分岐ではなく、planner の役割プロファイルで独立の pane を issue に立て、spec で seed する。第 3 lane(advisor)を意図的に足す(ADR 0004 の「必要になったら再訪」の再訪)。
2. **相談は完了契約の外**。meguri は agmsg を読まない・待たない・検証しない。run の成否は `result.json` + git 検証だけで決まる。
3. **advisor は ephemeral**。worker execute で spawn、worker run 終了で必ず reap、`keep_pane` に依存しない。書き込み可能な worktree を持たず、スロットを会計し、再起動では捨てて張り直す(#121 の横断制約)。

## 二つの「役割」を混同しない

この機能には別種の「役割」が二つ出てくる。spec 全体でこの区別を守る。

- **pane lane**(`src/store/panes.rs` の `role` 列):`author` / `review` の 2 値。ここに **`advisor` を第 3 の lane として足す**。pane の鍵は `(project, issue, lane)`(#92)。
- **routing role / loop_kind**(profile 選択の軸、`src/routing.rs`):`planner` / `worker` / … 。advisor は**新しい routing role を足さず**、planner 役割のプロファイルを借りる(解決順序は下記 3 — 直近の planner run が pin した profile を最優先で継ぐ)。routing 表(ADR 0003)は無変更。

## 決定と実装形

### 1. config `[collab]` セクション — `src/config.rs`

routing の「セクションの有無がスイッチ」規律(ADR 0003)に倣う。`RoutingConfig` と同型の小さな `Option` フィールドを `Config` に足す(`src/config.rs:78` `routing` の隣)。

```toml
[collab]
mode = "advisor"          # "off"(既定)| "advisor"
advisor_role = "planner"  # advisor が借りる routing role(既定 "planner")
# transport は agmsg 固定(v1)。skill パスも既定インストール先(~/.agents/skills/agmsg)固定。
```

- `collab: Option<CollabConfig>`。**セクション無し = 機能 off**(現状とバイト単位で同一)。
- `CollabConfig { mode: CollabMode, advisor_role: String }`。`CollabMode = Off | Advisor`(serde lowercase、既定 `Off`)。`advisor_role` 既定 `"planner"`。
- `mode = "off"` は明示 off(セクションはあるが不活性)。**有効化は `mode = "advisor"` の一点**。
- **`[collab]` は process-bound** — watch の毎 tick 再読込(#73/`ConfigReloader`)には乗せず、`mux.kind` / `[daemon]` と同類として起動時の値に pin し、reload での変更検知は restart-required warn にする(`meguri watch` の再起動で反映)。理由:`mode = "advisor"` の有効性は起動時の `collab::validate`(agmsg 検出、下記 2)と対でしか保証されないが、`ConfigReloader::poll` は validate を再実行しない(`routing::validate` も同じく起動時のみの「loud, early error surface」)。reload に乗せると、watch 稼働中に agmsg 未検出のまま `mode = "advisor"` を足す編集が通り、起動時なら明示エラーで落ちるはずの設定が run 時の best-effort spawn 失敗(下記 3)へ静かに化ける — 「silent fallback しない」(基準 3)が reload 経路で破れる。reload closure に `collab::validate` を足す代替案は退けた:reload の可否が agmsg の有無という**環境**に依存するようになり、「候補の拒否は内容だけで決まる(parse 失敗 / no projects)」という `ConfigReloader` の現在の性質を壊す上、routing すら reload で validate しない中で collab だけ検証するのは非対称。

### 2. 起動時の agmsg 検出 — `src/collab.rs`(新規)+ 起動時 validate

routing の CLI 検出(`routing::detect_command` = `command --version` が exit 0)を**流用**する。検出対象は PATH の `agmsg` ではなく **skill の実体 `~/.agents/skills/agmsg/scripts/version.sh`** にする:agmsg のランタイムは `~/.agents/skills/agmsg/scripts/` の bash script 群であり(PATH に入る npm の `agmsg` はインストーラの bootstrapper に過ぎない)、プロンプトが agent に叩かせるのもこの scripts なので(下記 4)、存在検出は実際に使うものを見る。`version.sh` は引数を無視して exit 0 で version を印字するので、`detect_command` に script のフルパスを渡すだけで流用できる。

routing の logic が `src/routing.rs` に、config 型が `src/config.rs` にあるのと同じ分業で、collab の runtime logic を **`src/collab.rs`** に置く:

- `pub fn validate(cfg: &Config, detect: &dyn Fn(&str) -> bool) -> Result<()>`:`mode = "advisor"` のとき上記 script が検出できなければ `bail!`(routing::validate と同じ「起動時に大きな音を立てて落ちる」流儀)。`advisor_role` が未知 routing role なら同様に startup error。`mode = "off"` / セクション無しは no-op。
- `routing::validate` を呼んでいる起動経路(`meguri watch` / `meguri run` の入口)から `collab::validate` も呼ぶ。**silent fallback しない**。
- `pub fn team_name(project_id, issue) -> String`:agmsg の共有 SQLite floor は複数 project を横断しうるので、team は `meguri-<project_id>-<issue>` で衝突を避ける(issue の「team = issue 単位」を project でスコープ)。
- prompt 断片ビルダ 2 つ(下記 4)。

### 3. advisor の spawn / reap — `src/engine/flow.rs`

**新 lane 定数** `ROLE_ADVISOR = "advisor"` を `src/store/panes.rs` に足す。

**opt-in は loop_kind ベースの共有判定にする**(plan review 指摘 2)。「どの loop が collab-advisor の対象か」を `Flavor` のメソッドにすると flow からしか見えない — スロット会計をする scheduler は `Vec<Arc<dyn Loop>>` と `RunRecord.loop_kind` しか持たず `Flavor` に触れないので、重み付けができない。だから判定は `src/collab.rs` の純関数に置き、flow と scheduler の両方が `loop_kind` から同じ真実を引く:

```rust
/// この loop_kind が collab-advisor の対象か。worker / spec-worker のみ true。
pub fn supports_advisor_loop_kind(loop_kind: &str) -> bool {
    loop_kind == crate::engine::worker::KIND
        || loop_kind == crate::engine::spec_worker::KIND
}
```

対象 loop の単一の真実はこの関数だけに集約する(`Flavor` にも `Loop` trait にもメソッドを足さない)。flow は spawn / reap の判定に、scheduler は下記スロット会計に、どちらも `run.loop_kind` を渡してこれを呼ぶ。

**spawn(execute 前)**:`drive` の `STEP_EXECUTE` ブロック(`flow.rs:339` 付近、worktree 再読込の直後・`execute(...)` の直前)で、`config.collab` が有効 かつ `collab::supports_advisor_loop_kind(&run.loop_kind)` のとき `ensure_advisor(deps, run, cp)` を呼ぶ。

- advisor pane は `(project, issue, ROLE_ADVISOR)` を鍵に確保するが、`ensure_pane` と違い **adopt / resume はしない — 捨てて張り直す**(#121):既存の live advisor pane が居たら `reaper::release_pane` で畳んでから新規 spawn する(resume 復帰・meguri 再起動を跨いだ生き残りも同じ扱い)。advisor の文脈は seed(spec 全文)だけで再現できるので、古い個体の途中状態を信用するより捨てる方が安い。同じ理由で advisor row の `agent_session_id` は常に空に保つ — `release_pane_record` の保存を advisor lane でガードして塞ぐ(下記 reap)ので、この respawn 時の畳みでも id は残らない。
- **profile は「plan を実際に作ったモデル」を継ぐ**:run の profile は初回 spawn 時に `runs.agent_profile` へ pin される(`flow.rs:771` `resolve_run_profile`、`src/store/runs.rs:148`)ので、その issue の**直近に成功した `advisor_role`(既定 planner)run の pin** を最優先で使う(store に issue × loop_kind の直近成功 run の `agent_profile` を引く読み取りクエリを 1 本足す)。`routing::resolve` をその場でやり直すだけだと、planning 後に routing 設定や auto 検出結果が変わった場合に plan 作者と別 profile になる。pin が無い(`meguri:ready` 直行で planner run が存在しない)/ pin した profile 名が config から消えている場合は、`routing::resolve(cfg, cfg.collab.advisor_role, detect)` の現在解決にフォールバックし `collab.advisor_profile_fallback` を emit する(run 本体の `resolve_run_profile` は「消えた pin は loud error」だが、advisor は best-effort なので落とさない)。いずれも **worker 自身の profile ピンとは無関係**。
- cwd は **worktree を持たせない**(#121:read-only は配線で保証)。advisor の cwd は repo の checkout ではなく、`<worktree_root>/<project>/advisor-<issue>` に spawn 時に作る**素の空ディレクトリ**(`git worktree` 登録なし)。書き込み可能な repo コピーがそもそも存在しないので、「コードを書くな」はプロンプトの願いではなく配線の事実になる。当初案の「review lane と同じ detached checkout」は退けた:detached でも checkout は書ける(read-only は慣習に過ぎない)し、run を持たない checkout は reaper の worktree 走査で issue を復元できず(`classify` は branch 名か `runs_for_worktree` に頼る — `src/engine/reaper.rs:150` 付近)orphan として残り続ける。素のディレクトリなら `git worktree list` に現れず reaper と干渉しない。削除は下記 reap で行い、万一漏れても空ディレクトリが残るだけで無害。相談の材料は seed の spec 全文と worker が送ってくる説明で足りる(コードレビューではなく要件充足の相談、という充て方と一致)。
- seed は spec 全文をプロンプトに inline(spec-worker が spec を読み込むのと同型、`spec_worker.rs:194-235`。読むのは meguri であり advisor に repo アクセスは要らない)。spec ファイルが無い `meguri:ready` 直行 issue では issue 本文を seed にフォールバック。
- **best-effort、ただし worker プロンプトと連動**:advisor spawn が失敗しても run は止めない(ADR 0006 原則 2)。`ensure_advisor` は spawn の成否を返し、execute のプロンプト組み立てはこれを見て **spawn が成功した時だけ**相談ブロック(下記 4)を append する。失敗時は `collab.advisor_spawn_failed` を emit し、相談ブロック無し(= collab off とバイト単位で同一)のプロンプトで進む — 存在しない advisor を「待機している」と worker に案内しない。

**スロット会計**(#121)— `src/engine/scheduler.rs`:advisor もサブスク枠で動く実 agent なので `scheduler.max_concurrent_runs` を消費させる。scheduler の予算判定(`discover` の `active.len() >= self.max_concurrent`)は run 数を数えているので、active を「run id の集合」から「run id → 重み」に持ち替え、**collab 有効(`mode = "advisor"`)かつ `collab::supports_advisor_loop_kind(&run.loop_kind)` が true な run を重み 2** として会計する(scheduler は `RunRecord.loop_kind` を持つので、上記の共有判定で `Flavor` に触れず重みを決められる)。判定は従来どおり「現在の消費 < 予算」で行い、加重後に予算を 1 だけ超え得る(`max_concurrent_runs = 1` でも collab 有効の worker run が飢えないための意図的な緩み)。advisor spawn が失敗しても予約は run の間維持する(保守的な過大予約を単純さで買う — best-effort の失敗を予算へ反映するには scheduler↔flow の連絡が要り、v1 では見合わない)。run 終了で重みごと解放。

**reap(run 終了)**:`run_flow` の終端 match(`flow.rs:257-308` の全 arm:成功 / needs-plan / decompose / 中断 / 失敗)で `release_advisor(deps, &run)` を呼ぶ。

- 中身は `reaper::release_pane(deps, issue, ROLE_ADVISOR, "worker run ended")` + advisor ディレクトリの削除(`remove_dir_all`、best-effort)。`keep_pane` を見ない(常に reap。ADR 0006 原則 3)。
- **session id は release の choke point で保存させない**(plan review 指摘 1/2)。`release_pane_record` は pane を殺す前に cwd から最新 session id を拾い `panes.agent_session_id` に保存する(`reaper.rs:480-488`、resume の可逆性)。advisor を畳む経路は 3 つ — `release_advisor`(run 終端)・reaper の `reclaim_panes`(安全網、下記)・`ensure_advisor` の respawn(捨てて張り直し、上記)— が、いずれもこの `release_pane_record` を通る。だから塞ぐのは 1 箇所:**`release_pane_record` の session 保存ブロックを `lane != ROLE_ADVISOR` でガードする** — advisor lane では拾わない・保存しない。これで 3 経路のどこで advisor pane を畳んでも session id は durable に残らない。呼び出し側で release 後に `save_pane_session(..., None)` する案(前回の `release_advisor` 限定クリア)は、まさに reaper 経路を漏らした通り経路ごとに取りこぼす;choke point 1 箇所のガードなら将来の caller も自動で守られる。session 保存の唯一の目的は resume の可逆性で、advisor は resume しないのだから、advisor で保存しないのは意味的にも正しい。
- collab 無効 or advisor pane 不在なら no-op(冪等・安全)。

**reaper 安全網** — `src/engine/reaper.rs`:pane 走査(`plan_panes`)は `list_panes` で**全 lane を role 無差別に**列挙する(`reaper.rs:370` 付近、`src/store/panes.rs:115`)ので、dead pane の mapping 掃除と issue close 時の回収は advisor lane にもコード変更なしで効く。worktree 走査の pane-alive guard(`classify` の `[ROLE_AUTHOR, ROLE_REVIEW]`、`reaper.rs:192`)は advisor が worktree を持たないので触らない。足すのは role 条件 1 つ:**issue が open でも、active run の無い advisor pane は `Reclaim`** にする(author / review が issue close まで生きるのと別規律)。この Reclaim は `reclaim_panes` → `release_pane_record` を通るが、上記のガード(`lane != ROLE_ADVISOR`)により advisor の session id はこの安全網経路でも保存されない。sweep は毎 tick + 起動時 recovery として回る(`scheduler.rs` の sweep 呼び出し)ので、run 終端 reap の取りこぼしも meguri 再起動を跨いだ生き残りも、ここで「捨てて張り直す」(#121)に収束する。正常経路では run 終端で先に消えている。

### 4. プロトコルはプロンプトに置く — worker 相談ブロック + advisor seed

agmsg 自身が「トランスポートは素朴に、プロトコルは各 agent のプロンプトに委ねる」設計(ADR 0006 の文脈)。meguri は agmsg を exec しない(起動時検出を除く)。両者のプロンプトに、使う script・team 名・相手 id・待ち方まで**具体的に**埋める。

- **v1 のプロトコルは raw script の明示呼び出しに固定する**。agmsg のランタイムは `~/.agents/skills/agmsg/scripts/` の bash script 群で、`send.sh <team> <from> <to> "<msg>"` は共有 SQLite に行を append するだけ、`inbox.sh <team> <id>` は未読を読んで既読化するだけ — 引数がすべて明示なので、per-project 登録(`join` / `whoami`)も delivery mode の hook 設定も `actas` の排他ロックも**一切要らない**。だから meguri は worktree の `.claude/settings.local.json` に手を入れず、agent の初回登録フロー(team / name の対話プロンプト)にも依存しない。`/agmsg` slash command が入っている環境でも、プロンプトは script 形を指示する(決定的で、agent の種類に依らない)。
- **turn スコープ方針(#121)— 助言は非時間依存と割り切る**。agmsg の delivery mode(monitor = リアルタイム push / turn = ターン間 pull)は使わず・設定せず、両者とも上記 `inbox.sh` の明示ポーリングで読む。worker は相談を送ったら返答を**有限時間だけ**ポーリングし、来なければ相談なしで進む。advisor の返答がいつ届くか・届かないかは run の成否に影響しない(ADR 0006 原則 2 の帰結)。リアルタイム助言が欲しくなったら delivery mode の導入をその時に再訪する(スコープ外)。
- **addressing**:team = `meguri-<project>-<issue>`、advisor の agmsg id = `advisor`、worker の id = `worker`。両プロンプトに team と相手 id を埋める。
- **worker execute プロンプトへの追記**(`src/engine/worker.rs:57` `execute_prompt`。**advisor spawn が成功した時のみ** append — 上記 3。collab 無効時・spawn 失敗時は**バイト単位で不変** = 受け入れ基準 1/4):

  > **相談してよい相手がいる。** この issue の spec を書いた助言者が agmsg team `meguri-<project>-<issue>` に id `advisor` で居る(相手は spec 全文を見ている)。実装の方針が spec の要件を満たしているか / ブレていないか迷ったら相談しろ:`~/.agents/skills/agmsg/scripts/send.sh meguri-<project>-<issue> worker advisor "<質問>"` で送り、返答は `~/.agents/skills/agmsg/scripts/inbox.sh meguri-<project>-<issue> worker` を 30 秒間隔ほどでポーリングして受け取れ。数分待って返答が無ければ相談なしで進め。相談は任意で、完了条件ではない。コードのレビューではなく要件充足とドリフトの相談に使え。

  spec-worker(`spec_worker.rs:194-235`)にも同じブロックを同じ条件で append。
- **advisor seed プロンプト**(`src/collab.rs` のビルダ、planner プロファイルで起動する pane の初期プロンプト):

  > お前は GitHub issue #N の spec を書いた本人(planner)だ。以下がその spec 全文だ。\n\n<spec 全文 or issue 本文>\n\n 実装中の worker が agmsg team `meguri-<project>-<issue>` から id `advisor` 宛に相談してくる。`~/.agents/skills/agmsg/scripts/inbox.sh meguri-<project>-<issue> advisor` を 30〜60 秒間隔で実行して新着を確認し続けろ(これがお前の待受だ。他の作業はするな)。相談が来たら**要件充足とドリフトの観点だけ**で簡潔に答え、`~/.agents/skills/agmsg/scripts/send.sh meguri-<project>-<issue> advisor worker "<回答>"` で返せ。worker が要件から外れていたら spec のどの部分に照らしてズレているかを指摘しろ。コードは書くな・コミットするな・ファイルを作るな(お前の作業ディレクトリは空で、書く場所は無い)。

- ポーリング間隔・待ち時間の数値はプロンプト文面の一部であってプロトコル定数ではない(実装時に文面ごと調整してよい)。meguri 自身は依然 agmsg を読まない・書かない・待たない。

## 変わらないもの(意図どおり)

- **完了契約は不変。** `result.json` + git 検証(clean tree / commits ahead / check pass)だけで成否が決まる。agmsg のやり取りの有無・内容は run に影響しない。
- **routing 表は無変更。** advisor は planner run の pin / `planner` role のプロファイルを借りるだけ。`KNOWN_ROLES` / `recommended_chain` に手を入れない。
- **author / review lane の寿命は無変更。** `keep_pane` の規律も、既定運用の author lane carryover(planner → worker が session を継ぐ)もそのまま。advisor はそれを置き換えず並走し、run 終端 reap の別規律だけを持つ。
- **impl-reviewer(#84)は無変更。** collab-consult(実装前・同期・worker 発・助言のみ)と impl-reviewer(PR 後・非同期・meguri 発・durable な review スレッド)は畳まず補完。
- **セクション無し = 現状と完全同一。** advisor を spawn せず、プロンプトはバイト単位で不変。
- **編成 DSL は作らない(#121)。** `[collab]` は `mode` / `advisor_role` の 2 キーに留める。役割ペアの宣言・多者編成・graph 定義といった編成の言語化は、worker↔advisor に次ぐ 2 例目の編成パターンが現れるまで封印。

## 受け入れ基準(acceptance criteria)

1. `[collab]` 無し(or `mode = "off"`)→ 現状とバイト単位で同一:advisor を spawn せず、worker / spec-worker の execute プロンプトが不変(テストで文字列一致を担保)。
2. `[collab] mode = "advisor"` かつ agmsg 検出あり → worker run で `advisor` lane の pane が `(project, issue, advisor)` に立ち、その profile は直近成功 planner run の pin を継ぐ(pin 不在 / 失効時は `advisor_role` の現在解決にフォールバック + event)。**spawn が成功した時だけ** worker プロンプトに team 名・相談先 id `advisor`・`send.sh` / `inbox.sh` の呼び出し形が入る。
3. `mode = "advisor"` + agmsg 未検出 → `meguri watch` / `meguri run` が起動時に明示エラーで停止(silent fallback しない)。`collab::validate` の単体テストで担保。
4. 完了契約は不変:run 時の advisor spawn が失敗しても worker run は止まらず(best-effort)、**その run の worker プロンプトに相談ブロックは入らない**(存在しない待機先を案内しない)。成否は `result.json` + git 検証だけで決まる(テストで担保)。
5. advisor は worker run の終了(成功 / needs-plan / decompose / 中断 / 失敗のいずれでも)で確実に reap され(pane + advisor ディレクトリ)、`keep_pane = "until-issue-closed"` でも常駐しない。resume・meguri 再起動では既存 advisor を adopt せず、捨てて張り直す。どの release 経路(run 終端 / reaper 安全網 / respawn の畳み)でも advisor row に `agent_session_id` は保存されない。
6. 対象は worker / spec-worker のみ。他 loop(planner / reviewer / fixer / …)は collab 有効でも advisor を spawn しない(`collab::supports_advisor_loop_kind` = false)。
7. advisor は書き込み可能な worktree を持たない:cwd は git 登録の無い空ディレクトリで、repo の checkout を一切渡さない(read-only は配線で保証)。
8. collab 有効時、worker / spec-worker の run はスケジューラ予算を 2 スロット消費する(`max_concurrent_runs = 1` でも単独なら起動できる)。collab 無効時の会計は現状と同一。
9. README(en/ja)が `[collab]` 節と「routing の次段」の位置づけを記述している。
10. `[collab]` は process-bound:watch 稼働中に config の `[collab]` を編集しても起動時の値に pin され(restart-required warn)、稼働中の効力は変わらない — 起動時 `collab::validate` を通っていない `[collab]` 設定が効力を持つ経路が存在しない(`ConfigReloader` の pin テストで担保)。

## テスト計画

- `src/collab.rs` の単体テスト:`validate`(off / advisor×検出あり / advisor×未検出→bail / 未知 advisor_role→bail)、`team_name`、プロンプト断片ビルダ(`send.sh` / `inbox.sh` の呼び出し形と team・id が入ること)。routing のテストが detector を closure 注入するパターン(`routing.rs:247` `only(...)`)をそのまま流用しサブプロセスを起こさない。
- `src/config.rs`:`[collab]` の parse(既定 off / mode / advisor_role)、セクション無しで `collab.is_none()`。`ConfigReloader` が `[collab]` の変更を pin して warn すること(既存 `reloader_pins_process_bound_settings` に collab ケースを追加、基準 10)。
- profile 解決:直近成功 planner run の pin あり→継ぐ / pin 無し→現在解決にフォールバック / pin 失効(config から消えた)→フォールバック + `collab.advisor_profile_fallback` emit(基準 2)。
- `worker.rs` / `spec_worker.rs`:execute プロンプトが collab off で不変(基準 1)、spawn 成功時のみ相談ブロックを含む(基準 2)、spawn 失敗時は off と文字列一致(基準 4)。既存の `prompt_invites_needs_plan` パターン(`worker.rs:196`)に倣う。
- flow 統合(FakeMux + FakeForge):collab on で advisor pane が spawn され run 終端で release される(pane + ディレクトリ、基準 5)、advisor spawn 失敗を注入しても run が成功しプロンプトが不変(基準 4)、resume 時に既存 advisor が捨てられ張り直される(基準 5)、collab off で pane 数不変(基準 1)。`supports_advisor_loop_kind` が false の loop_kind で spawn されない(基準 6)。あわせて `collab::supports_advisor_loop_kind` の単体テスト(worker / spec-worker のみ true)。
- scheduler:collab on で worker run が 2 スロット消費し、`max_concurrent = 1` でも単独起動できる(基準 8)。
- reaper:open issue × active run 無しの advisor pane が Reclaim になる(基準 5 の安全網)。その Reclaim 経路(`reclaim_panes` → `release_pane_record`)を通した後も advisor row の `agent_session_id` が空のまま(ガードの検証、指摘 1/2)。author / review lane では従来どおり session id が保存されること(ガードの非破壊)。

## 触るファイル

- `src/config.rs` — `[collab]` セクション(`CollabConfig` / `CollabMode`、`Config.collab: Option<_>`、pin 比較のため `PartialEq` derive)+ `ConfigReloader::poll` の process-bound pin に `collab` を追加(`mux` / `daemon` の隣、restart-required warn)
- `src/collab.rs` — 新規。`validate`(agmsg script 検出 = routing 流用)、`supports_advisor_loop_kind`(flow と scheduler が共有する対象判定)、`team_name`、seed / 相談ブロックのプロンプトビルダ
- `src/store/panes.rs` — `ROLE_ADVISOR = "advisor"` 定数
- `src/store/runs.rs` — 読み取りクエリ 1 本(issue × loop_kind の直近成功 run の `agent_profile`)
- `src/engine/flow.rs` — `ensure_advisor`(execute 前 spawn、捨てて張り直し、best-effort、成否を execute へ。対象判定は `collab::supports_advisor_loop_kind(&run.loop_kind)`)、`release_advisor`(run 終端 reap + ディレクトリ削除)
- `src/engine/worker.rs` / `src/engine/spec_worker.rs` — execute プロンプトへの相談ブロック append(advisor spawn 成功時のみ)。opt-in は `loop_kind` 側で決まるので `Flavor` メソッドは足さない
- `src/engine/scheduler.rs` — 加重スロット会計(`collab::supports_advisor_loop_kind` が true な collab 有効 run = 2)
- `src/engine/reaper.rs` — advisor pane の role 条件(open issue でも active run 無しなら Reclaim)+ `release_pane_record` の session 保存を `lane != ROLE_ADVISOR` でガード(全 release 経路で advisor に session id を残さない、指摘 1/2)。lane 列挙自体は `list_panes` が全 role を返すので変更不要
- 起動経路(`routing::validate` 呼び出し元)— `collab::validate` を併せて呼ぶ
- `README.md` / `README.ja.md` — `[collab]` 節(routing 節の後ろ、`README.md:279` 付近)
- `docs/adr/0006-collab-advisor-role-reembodiment.md` — 決定の記録(本 PR に同梱済み)
- テスト:`src/collab.rs` 単体 + flow 統合テスト(既存 `tests/` の FakeMux/FakeForge パターン)

## スコープ外(将来の話)

- 対象 loop の拡大(worker / spec-worker 以外)、多者 team、advisor 常駐。
- 編成 DSL・編成の config 言語化(#121 で 2 例目のパターンが出るまで封印)。
- meguri による agmsg history からの stall 検知、monitor/turn の delivery mode 導入(リアルタイム助言。「助言は非時間依存」の割り切りを崩す変更で、meguri は依然 agmsg を読まない前提とともに再訪)。
- transport の抽象化(agmsg 以外)、agmsg skill パスの config 化。
- agmsg のポーリング / 待ち受けエルゴノミクスの meguri 側最適化(v1 はプロンプトに書いた明示ポーリング)。
