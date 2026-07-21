# spec: issue #245 — 復旧不能セッションの resume ループを自動で断つ

> 使い捨ての実装 spec(ADR 0001)。durable な設計判断は ADR 0028 に、実装が land したらこの
> ファイルは消える。設計の出所は `docs/design/needs-human-friction-and-delivery-speed.md` §3-A・§P1。

## 深さの判断: design spec

**なぜ design 深度か**: 未決が多く、blast radius が広い。turn engine の無応答時の意味論を
変え(park→有界ローテーション)、mux トレイトに能力を1つ足し、pane registry のスキーマを
変える。これらは worker/planner/fixer/spec-worker/self-review/pr-reviewer の**全 lane が共有する
turn 経路**(`flow::run_turn` → `run_turn_in`)に効く。スキーマに触れるので **veto rule** により
migration & rollback 節も必須。

## 受け入れ基準(issue の3点)

1. context 100% + API 400 の恒久ループを fixture 化した統合テストで、**人手なしに fresh spawn へ
   復帰**する(needs-human が付かず、有界のターン数で success に到達)。
2. **agent 不在(素のシェル)の pane に nudge が打ち込まれない**。
3. **agent_quiet の needs-human エスカレーションのコメントに pane 末尾行が含まれる**。

## 設計と決定(A/B をここで畳む)

resume の健全性を4つのゲートに分ける(ADR 0028)。判定は一貫して「その session はまだ
会話できるか」。

### G1. resume 前の transcript サイズゲート

- **場所**: `ensure_pane`(pane mode)と `spawn_direct_process`(direct mode)で、保存済み
  session id を使って resume を決める直前。
- **判定**: `agent_session::transcript_len(session_root, worktree, session_id) -> Option<u64>`
  (新設)が `Some(bytes)` かつ閾値超過なら resume しない。`None`(場所が特定できない/未対応
  レイアウト)は「サイズ不明」で、G1 は**発火しない**(下記フォールバック)。
- **決定 D1 — 閾値の置き場所**: `AgentProfile` に `max_resume_transcript_bytes: u64`
  (`#[serde(default = …)]`、既定 `5 * 1024 * 1024`)。context window はプロファイル固有の
  性質(gpt と claude で違う)なのでプロファイル単位。`0` は「無効(サイズで弾かない)」。
  → **代替**: グローバル `[limits]` 1本。棄却 —— プロファイル毎に window が違うのに単一閾値では
  gpt に合わせると claude が過剰に fresh 化する。
- **決定 D1b — transcript locator の契約と profile の一致規則**(finding f4):
  - transcript の場所は **CLI 依存**である。現行の `…/projects/<munged worktree>/<id>.jsonl`
    レイアウトは Claude Code 固有で、`agent_session::latest_session_id`(record/reaper が使う
    scan)も既にこの前提に立っている。`transcript_len` はこの locator を**共有**し、単独で
    別レイアウトを推測しない。
  - **profile 一致規則**: `session_root` は必ず**その turn が spawn/resume に使うのと同じ
    profile**(lane の解決済み profile = `resolve_run_profile` のピン)から引く。G1・
    `record_agent_session`・reaper の3者が同一 profile の `session_dir` を見る。
    → これは現状の latent bug の是正でもある: `record_agent_session`(flow.rs)は
    `deps.config.agent`(既定 `[agent]`)から `session_root` を引いており、gpt など別 profile に
    ピンされた run では別ディレクトリの transcript を見失う。G1 を profile 単位にする以上、
    record/reaper も同じ profile に揃えないと「閾値は gpt、実際に見るのは既定 profile の
    transcript」というズレで context 100% を resume し続ける。**この是正を本 issue に含める**
    (`record_agent_session` の `session_root` を pinned profile 由来に変更。既定 profile の
    zero-config ケースでは同じ dir に解決されるので既存テストは不変)。
  - **locator の契約(`None` の定義)**: `transcript_len` は「その profile の `session_root` 直下の
    Claude レイアウト(`projects/<munged worktree>/<session_id>.jsonl`)に、渡された `session_id` の
    ファイルが在ればその `metadata().len()`、無ければ `None`」を返す**決め打ち**である(scan では
    なく直接参照)。したがって `None` は「まだ書かれていない」と「そのレイアウトを使わない CLI」の
    両方を同じ安全側に畳む —— named/custom CLI や別構造の `session_dir` はファイルがそこに無いので
    自然に `None` になる。将来 non-Claude CLI の transcript を測りたくなったら、その CLI 用の
    locator を足すまで `None`(=G1 無効)のままにするのが既定の契約。
  - **未対応レイアウト/未書き込みの安全な fallback**: `transcript_len` が `None` の場合、G1 は
    resume を止めず素通しし、`pane.resume_size_unknown` `{profile}` を emit する。サイズで測れなくても
    **G2(quiet カウンタ)が backstop** として context 100% を有界回数で断つ。「測れないなら無条件
    fresh 化」はしない(健全な resume まで捨ててしまうため)。
- **超過時の挙動**: resume せず、保存 session id を落とし、`pane.resume_skipped`
  `{reason:"transcript_too_large", bytes, limit, profile}` を emit、fresh spawn + full
  re-injection にフォールスルー(既存の「resume 失敗→full 再注入」経路をそのまま使う)。
  **quiet カウンタには触れない**(G2 とは独立の別トリガ)。

### G2. quiet ループの有界化

- **決定 D2 — 無応答を終端させるが、新 variant は `TurnOutcome` に足さない**(finding f3):
  `await_completion`(pane mode)の戻り型を新設の enum
  `AwaitOutcome { Completed(TurnResultFile), Stopped, PaneDied, AgentQuiet{tail}, AgentAbsent{tail} }`
  に変える。nudge を撃ち尽くした時点で **今までの無限 park をやめ**、pane 末尾を読んで
  `AgentQuiet`(または在否ゲートで `AgentAbsent`, G3)を返す。**`TurnOutcome`(公開・3 variant)は
  変えない**。`run_turn_in` が `AwaitOutcome` を受けて `TurnOutcome` に正規化する唯一の場所になる
  ので、`TurnOutcome` を match している約10か所(下記「call sites」)は**一切触らずコンパイルが
  通る**。
  - **決定 D2b — 公開境界**(finding f7): `await_completion` / `await_completion_direct` は現在も
    `pub` で、外部の統合テスト(`tests/turn_engine_test.rs`・`tests/turn_engine_direct_test.rs`・
    `tests/turn_tmux_test.rs`・`tests/notify_test.rs`)から直接呼ばれている。したがって
    `AwaitOutcome`(および G3 の `NudgeOutcome`)は **`pub` 型**にする(private 型を pub シグネチャに
    載せると private-in-public でコンパイル不能、かつ別クレートの test から名前を参照できない)。
    `TurnOutcome` が既に pub なのと同じ扱い。メソッドを `pub(crate)` に格下げして別の公開テスト境界を
    作る案は棄却 —— 既存の外部テスト4本を crate 内へ移す大改修になり、得るものが無い。
  → **代替1**: `TurnOutcome` に `AgentQuiet` を追加。棄却 —— cleaner/pr_reviewer/triage/self_review/
    flow の全 match が非網羅になりコンパイル不能(f3 の指摘そのもの)。網羅化のため各所に
    unreachable な arm を足すのは dead code。
  → **代替2**: park のまま crash recovery 再駆動に任せる。棄却 —— 再駆動が daemon 再起動頼みで
    13時間ループの構造が残る。
- **決定 D3 — カウンタの置き場所と意味**: `panes` 行に `quiet_falls INTEGER NOT NULL
  DEFAULT 0`。session をまたいで積む(quiet_loop で session を捨ててもリセットしない)。
  healthy な `Completed` turn で 0 に戻す。
- **決定 D4 — 遷移を共有 seam に集約し、各段で pane を明示 release する**(finding f1):
  分岐は `run_turn_in` が `AwaitOutcome` を受けた直後に1か所だけ置く(全 lane が
  `run_turn`/`run_review_turn`/`run_parallel_review_turn` 経由でここを通る)。閾値は名前付き定数
  `QUIET_ROTATE_AT = 2` / `QUIET_ESCALATE_AT = 3`。
  **重要**: `PaneDied` に翻訳するだけでは pane 行の `mux_pane_id` が生きたまま残り、次 dispatch で
  `ensure_pane` が **同じ壊れた pane を adopt して trigger を送るだけ**になる(session id を消しても
  adopt 経路は session を参照しないので fresh spawn も resume も起きない)。したがって各段で
  `reaper::release_pane`(= session 保存 → kill → `mark_pane_reclaimed` で `mux_pane_id` を NULL 化)を
  **明示的に呼び**、live pane を確実に畳んでから再 dispatch させる。
  - `Completed`/`Stopped`/`PaneDied` → 従来どおり(`quiet_falls` は Completed で 0 リセット)。
  - `AgentQuiet`/`AgentAbsent` → `quiet_falls` を N に加算し:
    - **N < 2(retry)**: `release_pane`(session id は保存されたまま)→ `TurnOutcome::PaneDied` を
      返す。次 dispatch は **live pane 無し + session 有り** なので `ensure_pane` の resume 経路
      (G1 サイズゲート適用)へ = 別プロセスで**同一 session を1回だけ resume 再試行**。
      `turn.agent_quiet {fall:N, action:"retry"}` を emit。
    - **N == 2(rotate)**: `release_pane` → 続けて `save_pane_session(None)` +
      `agent_session.cleared {reason:"quiet_loop"}`。次 dispatch は **live pane 無し + session 無し**
      なので **fresh spawn + full 再注入**(argv に resume_args を含まない)。
      `turn.agent_quiet {fall:N, action:"rotate"}` を emit。
    - **N >= 3(escalate)**: `release_pane`(session は人間用に保存)→
      `turn.awaiting_human {reason:"agent_quiet", fall:N, pane_tail:[…]}` を emit →
      **`Err(NeedsHuman(<説明 + sanitized tail>))`** を返す。`run_flow` の Err 経路が
      `flavor.escalate` を呼び、needs-human ラベル + コメントが付く(G4)。
  → **なぜ Err(NeedsHuman) に集約するか**: 全呼び出し側は `run_turn(...).await?` で伝播するので、
    escalate の呼び出しを各 lane に足さずに1か所で needs-human に落とせる。retry/rotate は
    既存の `PaneDied` arm を再利用でき、call site を増やさない。
  - **session-id の二重処理を避ける**: `AgentQuiet`/`AgentAbsent` の3段は **quiet 機械が session id を
    唯一管理**する(retry=保持 / rotate=クリア / escalate=保持)ので、これらの outcome では通常の
    `record_agent_session` を**呼ばない**。`record_agent_session` の `PaneDied if resumed → session
    クリア` 経路が走ると、retry が保持したいはずの session を消してしまうため。`record_agent_session`
    は genuine な `Completed`/`Stopped`/`PaneDied` のときだけ従来どおり走る。
- **call sites(f3 の網羅性)**: 触るのは `await_completion` のシグネチャと**その唯一の production
  呼び出し元 `run_turn_in`**、および `await_completion` を直接呼ぶ turn engine テスト
  (`tests/turn_engine_test.rs` / `tests/turn_tmux_test.rs`)だけ。`TurnOutcome` を match する
  flow.rs(1925/2100/2277 系)・self_review.rs(840/1144/1191/1319)・cleaner.rs(617)・
  pr_reviewer.rs(672)・triage.rs(949)は `run_turn`/`run_review_turn` の戻り(正規化済み
  `TurnOutcome`)を見るので**不変**。`await_completion_direct` も `AwaitOutcome` を返すが
  quiet/absent は生成しない(direct は nudge 無し)。
- **direct mode**: nudge が無く AgentQuiet は起きない。resultless exit → `AwaitOutcome::PaneDied` →
  `record_agent_session` の「PaneDied if resumed で session を落とす」既存経路が既にループを断つ。
  よって G2 の quiet 機械は pane mode 専用。direct mode は G1 の恩恵だけ受ける。

### G3. agent の在否ゲート

- **決定 D5 — 在否は mux の一級能力**: `Multiplexer` に
  `async fn agent_present(&self, pane: &PaneId) -> MuxResult<bool>` を追加。既定実装は
  `Ok(true)`(在否を判別できない mux は挙動不変)。
  - **herdr**: native。pane に agent integration が登録されているか(agent_status の有無)で答える。
  - **tmux**: `#{pane_current_command}` を見て、shell 名(`zsh`/`bash`/`sh`/`fish` …)なら不在。
  - **FakeMux**: テスト用に設定可能なフラグ。
  → **代替**: `AgentState::Unknown` を「不在」とみなす。棄却 —— Unknown は tmux で生きた agent でも
    起きる(settle 前・非ブロック UI)ので在否に流用すると誤検出する(ADR 0028)。
- **決定 D5b — nudge は在否ゲート付きの必須メソッドにする**(finding f2/f8/f9):
  「`agent_present` で確認 → `send_line` で送信」は2回の mux 呼び出しなので TOCTOU が残る。これを
  塞ぐため、nudge 専用メソッド `Multiplexer::nudge(&self, pane, text) -> MuxResult<NudgeOutcome>`
  (`NudgeOutcome ∈ {Delivered, AgentAbsent}`、いずれも `pub`)を足し、`await_completion` の nudge は
  これを使う。**契約**: 実装は送信前に「agent が pane の前面プロセスである」ことを検証し、agent
  不在なら `text` を**届けず** `AgentAbsent` を返す。
  - **決定 D5b-1 — 既定実装を持たせない(finding f9)**: `nudge` は **required trait method**
    (デフォルト body 無し)にする。`send_line` へ委譲する既定を置くと、override しない mux が
    在否を検査せず shell へ送ってしまい D5b の契約に反する。実装は herdr/tmux/fake の3つだけなので
    全実装に原子性を義務付けても負担は小さい。
  - **決定 D5b-2 — 完全な原子性は herdr が担い、tmux は best-effort + 自己修復で被害ゼロ(finding f8)**:
    真の byte 単位原子性(「前面プロセスが agent のときだけ送る」)を tmux 単独で保証するのは、tmux に
    「PID 宛て送信」が無いため不可能である。よって保証を層で分ける:
    - **herdr(meguri の本番 mux)**: agent integration 宛ての送信を使い、agent が detach していれば
      サーバ側で no-op/失敗する。**真に原子的** —— 本番経路に競合窓は無い。
    - **tmux(ローカル/開発の fallback)**: `send-keys` を単一の `if-shell` でガードし
      (`tmux if-shell -t <pane> '<pane_current_command が shell でない判定>' 'send-keys …'`)、
      判定と送信を tmux サーバの1コマンド dispatch に畳んで窓を最小化する。それでも判定〜送信の間に
      agent が exit する残余窓は原理的にゼロにできない。
    - **設計は原子性に依存しない(被害の有界化)**: 仮に tmux で1回 nudge が shell に漏れても、次の
      poll で `agent_present` が偽 → `AwaitOutcome::AgentAbsent` → G2 の有界機械が release+rotate で
      **自己修復**する。漏れは高々シェルの1行ノイズで、#245 が断とうとしている「13時間ループ」には
      発展しない。つまり厳密な原子性が要るのは体裁の問題だけで、それは本番の herdr が満たす。
  - **検証可能性(finding f8）**: 「不在なら1文字も届けない」は FakeMux で決定的に検証する
    (下記 Test strategy)。tmux の `if-shell` ガードが実際に送信を抑止することは mux_tmux テストで、
    残余窓の自己修復は受け入れ2の統合テストで確認する。
  - **FakeMux**: agent-absent フラグが立っていれば `AgentAbsent` を返し `sent_lines` に何も積まない
    (テストで決定的)。
- **呼ぶ場所は2か所**:
  - **(a) adopt ゲート**: `ensure_pane` が生きた pane を adopt する条件を
    `pane_alive && agent_present` にする。生きているが agent 不在なら adopt せず、
    `release_pane`(session 保存 + kill + reclaim)してから respawn 経路(G1 のサイズゲート適用)へ。
    `pane.agent_absent {lane}` を emit。→ **最初の trigger を素のシェルに打ち込む事故**を断つ。
    (adopt 時の1回だけの確認なので、ここは原子性より「生きた shell を掴まない」ことが要点。)
  - **(b) nudge 時**: `await_completion` の stagnation 分岐(`activity_clock >= idle_grace`)で
    `nudge`(D5b の原子的 API)を呼ぶ。戻りが `AgentAbsent` なら **1文字も届いていない**ので、
    nudge せずに `AwaitOutcome::AgentAbsent{tail}` を返す(G2 の機械に載る)。
    `turn.agent_absent {turn_id}` を emit。→ **turn 途中でシェルへ nudge** を、確認→送信の窓ごと断つ。
  - boot 中の誤検出は idle_grace(既定90秒)経過後にしか (b) を評価しないことで避ける。

### G4. 診断同梱(read するが裁定しない)— sanitize してから出す

- **決定 D6 — pane tail を N=25 行**(`app.rs` の既存 `read_tail(&pane, 25)` に合わせる)。ただし
  raw のままイベントや Forge コメントへ流さない(finding f6: 画面上の credential・PII・issue 由来の
  任意 Markdown が外部へ漏れる/長大な1行や ``` による fence 脱出が起きる)。
- **決定 D6b — sanitize パイプライン**(公開先の前に必ず通す。`src/turn/prompts.rs` に
  `sanitize_pane_tail(lines) -> Vec<String>` と `fence_for_comment(lines) -> String` を新設):
  1. **制御文字/ANSI 除去**: ESC シーケンス・非表示制御文字を落とす(pane scrollback は色コード等を含む)。
  2. **秘密のレダクション**: 既知パターン(`sk-…`/`ghp_…`/`AKIA…` 等の token 形、`Authorization:`・
     `password=`・`api[_-]?key` 行、長い base64/hex ラン)を小さな denylist で `‹redacted›` に置換。
     完全ではない前提で、後段のバイト上限と併せて被害面を絞る。
  3. **バイト上限**: 1行あたり上限(例 200 bytes、超過は末尾 `…` で切詰)+ tail 全体の総量上限
     (例 2 KiB、超えたら古い行から落とす)。長大1行 DoS を防ぐ。
- **決定 D6c — 公開先ごとの扱い**:
  - **event(`turn.awaiting_human.pane_tail`)**: ローカル sqlite。上記 sanitize 済みの
    `Vec<String>` を構造化して格納(Markdown 化はしない)。
  - **Forge コメント(外部・公開されうる)**: 同じ sanitize 済み行を `fence_for_comment` で包む。
    fence は**本文中の最長バックティック連より長い**フェンスを選び(``` を含む行があっても
    脱出させない)、まず制御文字が無いことを前提に ` ```text ` ブロックへ入れる。バックティックの
    残存リスクを二重に潰すため、本文中の 3 連以上バックティックは無害な字面へ置換してから包む。
- **不変条件(overview / ADR 0025・0026)**: sanitize 済み tail も**人間の診断のための read** で
  あり、成否裁定・finding 生成には一切使わない。パースしない。整形して人間向けに出すだけ。

## 触るファイル

- `src/agent_session.rs` — `transcript_len(session_root, worktree, session_id) -> Option<u64>` を
  追加(既存 locator を共有、未対応レイアウトは `None`)(G1/D1b)。
- `src/config.rs` — `AgentProfile::max_resume_transcript_bytes` + 既定値関数(G1/D1)。
- `src/mux/mod.rs` — `pub enum NudgeOutcome`、`Multiplexer::agent_present`(既定 `Ok(true)`)と
  `Multiplexer::nudge(pane, text) -> MuxResult<NudgeOutcome>`(**required method・既定 body なし**、
  D5b-1)を追加(G3/D5・D5b)。
- `src/mux/herdr.rs` / `src/mux/tmux.rs` / `src/mux/fake.rs` — `agent_present` と `nudge` 実装
  (herdr=agent 宛て原子的送信 / tmux=`if-shell` ガード付き `send-keys` / fake=absent フラグ)(G3)。
- `src/turn/mod.rs` — `pub enum AwaitOutcome`(`AgentQuiet{tail}`/`AgentAbsent{tail}` を含む)を新設し
  `await_completion`(`pub`)の戻り型に。nudge 撃ち尽くし時の返却 + 在否チェックで `nudge` を使用
  (G2/D2・D2b・G3(b))。**`TurnOutcome`(公開)は不変**。`await_completion_direct`(`pub`)も
  `AwaitOutcome` を返す(quiet/absent は生成しない)。両型が pub なので外部統合テストから参照可能。
- `src/turn/prompts.rs` — `sanitize_pane_tail` / `fence_for_comment` と、needs-human 理由へ
  tail ブロックを畳むヘルパ(G4/D6b・D6c)。
- `src/engine/flow.rs` — `ensure_pane` の adopt ゲート(G3(a))、`ensure_pane` /
  `spawn_direct_process` の resume 前サイズゲート(G1)、`run_turn_in` で `AwaitOutcome` →
  `TurnOutcome` 正規化 + quiet 遷移集約 + **各段の `release_pane`**(G2/D4)、
  `record_agent_session` の `session_root` を **pinned profile 由来に是正**(G1/D1b)。
- `src/store/panes.rs` + `src/store/migrations/0017_pane_quiet_falls.sql` +
  `src/store/mod.rs`(migration 登録)— `quiet_falls` 列と bump/reset メソッド(G2/D3)。
- `tests/fixtures/fake_agent.sh` — 「resume 時に恒久 400 を出して結果を書かない」モードと
  「exit して素のシェルに落ちる」モードを追加(下記テスト strategy)。
- `tests/turn_engine_test.rs` / `tests/turn_tmux_test.rs` — `await_completion` の戻り型変更に追随
  (`AwaitOutcome`)。それ以外の `TurnOutcome` match 箇所は不変(G2 の call sites 参照)。
- `tests/resume_test.rs` ないし新規 `tests/session_health_test.rs` — 統合テスト。

## Architecture impact / alternatives

- **impact**: turn の無応答が「無限 park」から「有界ローテーション → needs-human」へ変わる。
  これは全 lane の turn 経路が共有する意味論の変更だが、集約点(`run_turn_in` と
  `await_completion`)が1か所ずつなので分散はしない。mux トレイトは能力が1つ増えるだけで
  既存実装は既定 `true` により不変。
- **alternatives**(棄却理由は各決定に併記):グローバル閾値(D1)/ park 継続 + 再駆動待ち(D2)/
  Unknown 相乗り(D5)。いずれも取りこぼしか、13時間ループの構造温存につながる。

## Migration & rollback(スキーマに触れるため必須)

- **migration `0017_pane_quiet_falls.sql`**:
  `ALTER TABLE panes ADD COLUMN quiet_falls INTEGER NOT NULL DEFAULT 0;`。
  加算のみ・データ移行不要(初期値0)。`schema_migrations` ガードで冪等。
- **rollback(旧バイナリを再デプロイ)**: 列は休眠するだけ。`upsert_pane` /
  `upsert_pane_session` は明示列リストの INSERT + `ON CONFLICT DO UPDATE SET` なので、
  DEFAULT 付き追加列は旧バイナリの書き込みでも問題にならない。データ損失なし・不可逆操作なし。
- **設定 `max_resume_transcript_bytes`**: `#[serde(default)]` で後方互換。`AgentProfile` は
  `deny_unknown_fields` を使っていないので、旧バイナリは未知キーを無視する。
- **不可逆な運用リスク**: なし。session を捨てる操作は transcript ファイル自体を消さない
  (id を pane row から外すだけ)ので、人間が後から `claude --resume <id>` で辿ることは可能。

## Observability

- 新イベント: `pane.resume_skipped` / `pane.resume_size_unknown` / `pane.agent_absent` /
  `turn.agent_absent` / `turn.agent_quiet`(action=retry|rotate)。既存:
  `agent_session.cleared`(reason=quiet_loop)/ `turn.awaiting_human`(reason=agent_quiet,
  sanitized pane_tail 付き)/ `escalation.raised`。
- これらから「session rotate 回数」「quiet_falls 分布」が events テーブルだけで出せる。
  設計書 §5 の `meguri stats review` への集計面追加は **本 issue のスコープ外**(計測は
  ADR 0026 の枠で別途)。ここではイベントを出すところまでを担保する。

## Test strategy

- **unit**:
  - `agent_session::transcript_len`: サイズ算出、存在しない id は `None`、未対応レイアウトは `None`。
  - `sanitize_pane_tail` / `fence_for_comment`(G4/f6): (a) `ghp_…` 等がレダクトされる、(b) ``` を含む
    行が fence を脱出しない、(c) 1万文字の1行がバイト上限で切詰、(d) ANSI/制御文字が除去される。
  - `run_turn_in` の quiet 遷移(retry→rotate→escalate、Completed でリセット)を FakeMux で。
  - tmux `agent_present` の `pane_current_command` 分類(shell 名↔agent コマンド)。
  - `nudge` の在否ゲート契約(f2/f8/f9): FakeMux で agent-absent 時に `NudgeOutcome::AgentAbsent` を
    返し `sent_lines` が空のまま(「不在なら1文字も届けない」を決定的に検証)。tmux 実装は
    `if-shell` ガードにより shell 前面時に `send-keys` を実行しないこと(mux_tmux テストに追加)。
    `nudge` は required method なので「override 忘れで shell へ素通し」はコンパイル時に不可能
    (デフォルト実装が無い)。残余窓の自己修復(漏れても次 poll で AgentAbsent→rotate)は受け入れ2で。
  - migration の冪等性(既存 `migrations_apply_and_are_idempotent` に追随)。
- **G1 を独立に固定するテスト(finding f5)** —— G1 を落としても G2 rotate で通ってしまう抜けを塞ぐ:
  - **G1 pane**: 保存 session id + 閾値超過サイズの transcript を seed し、1 turn 走らせて
    **`spawned_commands()[0]` に resume_args/`--resume` が**含まれず**、full-prompt trigger を
    carry すること、`pane.resume_skipped` が emit されることを assert(quiet を1回も起こさない=
    G1 単独の証明)。
  - **G1 direct**: 同条件を direct mode で。`spawn_direct_process` の argv に resume_args が無いこと
    (`direct.spawned {resumed:false}`)を assert。
  - **未対応レイアウト fallback**: `transcript_len == None` の profile で resume が素通しされ
    `pane.resume_size_unknown` が出ること。
- **統合(実 tmux + `fake_agent.sh`)**:
  - **受け入れ1(G2 backstop)**: fake_agent を「resume されたら恒久 400 を表示し結果を書かない」に。
    **G1 を無効化した状態**(閾値 0 か transcript を置かない)で走らせ、400→quiet→**rotate で
    実際に fresh spawn(resume_args を含まない spawn)が起きる**こと、needs-human なしで有界ターン内に
    success へ到達することを assert(#222 の fixture 化)。fresh spawn を argv で直接確認するのが
    f1 の要点 —— 「session id を消しただけで pane が残り adopt される」退行を検出する。
  - **受け入れ2(在否)**: fake_agent を「exit して素のシェルに落ちる」に。`sent_lines`(または
    pane 内容)に nudge 行が入らないこと、pane が release + respawn されることを確認。
  - **受け入れ3(診断)**: quiet を3回起こし、needs-human コメントに **sanitize 済み** tail 行が
    含まれ、かつ埋め込んだ擬似 credential がレダクトされていることを FakeForge の記録に対して assert。
- FakeMux 拡張: `set_agent_present`(不在)/ tail シード / `nudge` の absent 返却、不在時に
  `send_line`・`nudge` のどちらも pane へ届けないこと。

## スコープ外(この issue では触らない)

- 設計書の P2〜P6.7(冪等 escalation・anchor 照合・impl fixer・重複デリバリ・sweep 可観測化・
  stop の claim 掃除)。それぞれ別 issue。
- `meguri stats review` への rotate 回数の集計面(計測は ADR 0026 の枠で別途)。
