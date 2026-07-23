# issue-245 spec — 復旧不能セッションの resume ループを自動で断つ

死んだ会話に何度も話しかけても、返事は返らない。gpt プロファイルの
pr-reviewer が context 100% に達すると以後の入力は全て API 400 になり、
`ensure_pane` はその死にセッションを無条件に `--resume` し続ける。
nudge→agent_quiet→recover が13時間ループした(#222、#235 で再発)。
さらに agent プロセスが死んで pane が素の zsh に落ちても、pane 生存だけを
見る現行検出は nudge をシェルへ打ち込む(`zsh: command not found: ...`)。

この spec の決定は一行で書ける。**resume の条件を「pane が開くか」から
「その会話がまだ生きているか」へ変え、生きていないと分かったセッションは
人手なしに捨てて fresh spawn へ落とす。**

## なぜ design spec か（深さの理由）

永続状態（`panes` テーブルに1列追加）と公開契約（`Multiplexer` トレイト・
`TurnOutcome` enum・`AgentProfile` config）を触る。veto ルールにより
migration & rollback は必須、blast radius も turn engine 全体に及ぶため
design spec とする。

## 三つの独立した栓

故障モードは二つ（①死にセッションへの resume ②agent 不在 pane への nudge）
あり、対策は三つの独立した機構になる。どれも他に依存せず、単体で意味を持つ。

### 栓1: resume 前の transcript サイズゲート（①の一次対策）

`ensure_pane`（`src/engine/flow.rs:1333`）と `spawn_direct_process`
（同 1990）は、pane 行の `agent_session_id` があれば無条件に `--resume` する。
その前に transcript ファイルのサイズを見て、閾値超過なら resume を諦めて
fresh spawn + full re-injection に落とす。プロンプトは自己完結しており
文脈の再構築は要らない（overview の完了コントラクト）。

- transcript の実体は `agent_session.rs` が既に知っている:
  `<session_root>/projects/<munged cwd>/<session-id>.jsonl`。
  ここに `transcript_path()` と `transcript_len() -> Option<u64>` を足す。
- ゲートは resume を試みる直前（`ensure_pane` の session_id 取得後、
  `spawn_agent_pane(..., Some(id))` の前）。超過なら saved session を消し、
  `agent_session.cleared { reason: "transcript_oversize", bytes, limit }` を
  emit して fresh spawn へ落ちる（既存の resume 失敗フォールバックと同じ経路）。
- 8.8MB の実例は 5MB 閾値で初回 resume すら起きず、400 ループは発生前に消える。

### 栓2: agent 不在 pane では nudge しない（②の対策）

turn engine（`src/turn/mod.rs`）の stagnation 経路は、quiet と判定したら
`send_line(pane, nudge)` を打つ。その前に「pane に agent プロセスがいるか」を
mux に問い、不在が確定したら nudge せず即 `PaneDied` を返す。

- `Multiplexer` トレイトに既定実装付きメソッドを足す:
  ```rust
  /// Some(true)=agent 稼働中 / Some(false)=素のシェル（agent 不在）/
  /// None=判定不能（mux が見分けられない）。
  async fn agent_present(&self, pane: &PaneId) -> MuxResult<Option<bool>> {
      let _ = pane; Ok(None)
  }
  ```
- turn engine は nudge を打つ直前だけこれを呼ぶ（毎 poll ではない — 無駄打ちが
  起きうるのはこの一点だけ）。`Some(false)` のとき
  `turn.pane_died { turn_id, reason: "agent_absent" }` を emit して
  `TurnOutcome::PaneDied` を返す。`None`/`Some(true)` は従来どおり nudge。
- `PaneDied` は `record_agent_session`（`flow.rs:2094`）で resumed 時に
  session をクリアするので、次 spawn は fresh になる。

### 栓3: 同一セッションの agent_quiet を数え、2回で捨て3回で人間へ（①の恒久対策）

閾値未満の transcript でも死にセッションはありうる。同一 lane で agent_quiet が
繰り返したらセッションが壊れている証拠なので、回数で機械的に処理する。

現状 `await_completion` は quiet を検出すると `turn.awaiting_human(agent_quiet)`
を emit して**その場で永久に park する**（return しない）。ループが回るのは
orchestrator 再起動の `recover` が同じ session を resume し直すから。これを
断つには quiet を**返す**ようにして、flow 層で回数に応じて分岐させる。

- turn engine: nudge 上限到達 → park の代わりに
  `TurnOutcome::AgentQuiet { tail: Vec<String> }` を返す（`read_tail(pane, 30)`
  を1回読んで同梱。これが栓4の診断になる）。
- `panes` に `quiet_strikes INTEGER NOT NULL DEFAULT 0` を足す。
- flow 層に共有ヘルパ `handle_agent_quiet(deps, run, lane, tail) -> Result<StepFlow>`:
  - `n = bump_pane_quiet_strikes(...)`（+1 して新値を返す）
  - `n < 2` → `StepFlow::Interrupted`（session 温存 → 次 dispatch で再 resume。
    一過性の沈黙に猶予を1回与える）
  - `n == 2` → saved session を消し
    `agent_session.cleared { reason: "quiet_loop" }` を emit → `Interrupted`
    （次 dispatch は fresh spawn）
  - `n >= 3` → `Err(NeedsHuman(reason))`（reason に tail を同梱 → 既存の
    `flavor.escalate` 経路がコメントを投稿）
- reset: `TurnOutcome::Completed` を `record_agent_session` で処理するとき
  `reset_pane_quiet_strikes(...)` で 0 に戻す。**session を消す `n==2` では
  reset しない** — reset すると fresh 後にまた quiet ったとき strike が 1 に
  戻り、3（needs-human）へ永遠に到達しないため。
- 定数: `QUIET_STRIKE_CLEAR = 2`、`QUIET_STRIKE_HUMAN = 3`。

### 栓4: agent_quiet の escalation に pane tail を同梱（受け入れ基準3）

栓3の `n >= 3` で `NeedsHuman` に載せる reason へ、pane 末尾 N=30 行を
fenced block で埋める。読むのは診断のためで**成否裁定には使わない** —
ADR 0026 の「read するが裁定しない」と同じ立て付け。overview の「画面を読んで
成否判定しない」原則は破らない。`flavor.escalate` はこの reason をそのまま
コメント本文にするので、追加の配線は要らない。

## 決めた論点（A/B を後回しにしない）

1. **quiet_strikes の置き場**: events 由来の再計算ではなく `panes` の実カラム。
   設計書が「pane 行に数え」と明示、参照が O(1)、reset も素直。
2. **strike の reset 契機**: 成功 turn 完了時のみ。`n==2` の session クリアでは
   reset しない（上述の到達性の理由）。
3. **agent 不在の信号**: `AgentState` に variant を足さず、`agent_present() ->
   Option<bool>` の新メソッド。`AgentState` の全 match を触らずに済み、
   「判定不能(None)」と「不在確定(Some(false))」を明確に分けられる。rust ルール
   「新しい振る舞いはまずトレイトに足す」に沿う。herdr は `pane get` の
   前景プロセス/agent バインドから、tmux は best-effort（末尾がシェルプロンプト
   なら `Some(false)`、判別不能なら `None`）、fake は setter で返す。
4. **transcript 閾値の置き場**: `AgentProfile` の per-profile フィールド
   `resume_transcript_limit_bytes: u64`（default 5*1024*1024、`0` で無効）。
   設計書が「プロファイル毎、既定 5MB」と指定。context window の小さい gpt
   プロファイルだけ厳しくできる。
5. **transcript 超過の event 名**: 新設せず `agent_session.cleared` を
   `reason: "transcript_oversize"` で再利用（session を捨てる点で quiet_loop と
   同種）。
6. **agent_quiet を返り値にする是非**: park 継続ではなく返す。park のままだと
   同一プロセス内で strike 2/3 へ進めず、自動復帰が起きない（受け入れ基準1が
   満たせない）。
7. **`TurnOutcome::AgentQuiet` を全 match 箇所へ**: `flow.rs`（execute/validate）・
   `pr_reviewer.rs`・`self_review.rs`・`cleaner.rs` の各 `match TurnOutcome` で
   共有ヘルパ `handle_agent_quiet` を呼ぶ。pr-reviewer lane が実際の事故現場
   なので、ここを外さない。

## 変更箇所

- `src/agent_session.rs` — `transcript_path()` / `transcript_len()` を追加。
- `src/config.rs` — `AgentProfile.resume_transcript_limit_bytes`（default 5MiB）。
- `src/mux/mod.rs` — `Multiplexer::agent_present()`（既定 `Ok(None)`）。
- `src/mux/herdr.rs` / `src/mux/tmux.rs` — `agent_present` 実装。
- `src/mux/fake.rs` — `agent_present` を setter で制御（`set_agent_present`）。
- `src/turn/mod.rs` — nudge 前の `agent_present` 検査 → `PaneDied`；
  quiet 上限で `TurnOutcome::AgentQuiet { tail }` を返す（park しない）。
- `src/engine/flow.rs` — `ensure_pane` / `spawn_direct_process` に transcript
  ゲート；`handle_agent_quiet` ヘルパ；`record_agent_session` で Completed 時
  reset・AgentQuiet の strike 処理；`run_turn` の `outcome_str` 追加。
- `src/engine/pr_reviewer.rs` / `self_review.rs` / `cleaner.rs` — 新 variant を
  `handle_agent_quiet` へ配線。
- `src/store/panes.rs` — `PaneRecord.quiet_strikes`、`bump_/reset_` メソッド。
- `src/store/migrations/0017_pane_quiet_strikes.sql`（+ `mod.rs` の MIGRATIONS）。
- `docs/adr/00NN-session-health-converse-not-just-open.md` — 決定の記録
  （実装 PR に同梱。次の空き番号）。

## architecture impact

turn engine の契約に `TurnOutcome` の1 variant が増える（park→return の意味変更を
含む）。mux 抽象に1メソッド増える。この2つは公開契約だが、いずれも既定実装
（`agent_present` は `Ok(None)`、AgentQuiet は各所で `handle_agent_quiet` に集約）で
後方互換に足せる。resume 判定は「開くか」から「会話できるか」へ拡張される
（設計書 §P1 の核）。ADR 0023 の異種モデル路線で小さい context window の
プロファイルが増えるほど効くので、前提整備として位置づける。

## alternatives considered

- **AgentState に `Absent` variant を足す案**: `AgentState` を読む全箇所が新 case を
  抱える。tmux/Unknown との境界も曖昧（Unknown=判定不能 と Absent=不在確定 が
  混ざる）。→ `agent_present -> Option<bool>` の方が意味が締まるので却下（論点3）。
- **quiet_strikes を events から再計算する案**: スキーマ変更を避けられるが、
  reset 契機・session 境界の扱いがイベント解釈に依存して脆い。→ 実カラム採用。
- **strike 1 で即セッションを捨てる案**: 一過性の沈黙まで fresh spawn を強いる。
  設計書が「2回目で破棄」と明示。→ 1回は resume 続行の猶予を残す。

## migration & rollback

- **migration**: `0017_pane_quiet_strikes.sql` は
  `ALTER TABLE panes ADD COLUMN quiet_strikes INTEGER NOT NULL DEFAULT 0` の
  追加列のみ。backfill 不要（既存行は 0 始まり）。`panes` の読みは
  `SELECT *`（`panes.rs:146`）なので列追加は `pane_from_row` に1行足すだけ。
  config の新フィールドは `#[serde(default)]` で既存 `meguri.toml` を素通し。
- **rollback**: 追加列は無害なので旧バイナリに戻しても放置で足りる（読まれない）。
  `TurnOutcome::AgentQuiet` / `agent_present` は新バイナリ内だけの型で外部契約に
  漏れない。破壊的 down-migration は不要（このリポジトリは forward-only）。
  緊急時は `resume_transcript_limit_bytes = 0` で栓1を、strike 分岐は定数を
  十分大きくすれば実質無効化できる（キルスイッチ相当）。

## observability

- `agent_session.cleared { reason: "transcript_oversize" | "quiet_loop" }`
- `turn.pane_died { reason: "agent_absent" }`
- `turn.awaiting_human` は栓3の `n>=3` 経由の escalation に統一（park 廃止）。
- session rotate 回数の stats 化（設計書 §5）は本 issue のスコープ外。ここでは
  events を出すに留め、集計は測定 issue 側に委ねる。

## test strategy

- **単体（FakeMux/FakeForge）**:
  - 受け入れ基準2: `set_agent_present(Some(false))` の pane で quiet になっても
    `sent_lines` に nudge が無く、outcome が `PaneDied` であること。
  - 栓3: 同一 lane で quiet を3回起こし、1回目 Interrupted / 2回目
    `agent_session.cleared(quiet_loop)`＋session 消去 / 3回目 needs-human＋
    コメントに tail、を検証。成功 turn で strike が 0 に戻ること。
  - 栓1: 閾値超の transcript を seed し、resume されず fresh spawn（`--resume` を
    含まない spawn コマンド）になること。`resume_transcript_limit_bytes=0` で
    ゲート無効も1本。
- **統合（実 tmux + `tests/fixtures/fake_agent.sh`）**: 受け入れ基準1。
  fake_agent が resume 時に result を書かず沈黙 → 閾値超 transcript を事前配置
  →（人手なしに）fresh spawn が result を書いて復帰することを通しで確認。
  新規 `tests/resume_test.rs` に追記、または `tests/turn_engine_test.rs` を拡張。

## 受け入れ基準

1. 400 恒久ループを fixture 化した統合テストで、人手なしに fresh spawn へ
   復帰する（栓1 が初回 resume を止める／栓3 が2回目で session を捨てる）。
2. agent 不在 pane（`agent_present == Some(false)`）に nudge が打ち込まれない。
3. agent_quiet の escalation コメント（strike 3）に pane tail が含まれる。
4. 成功 turn 完了で `quiet_strikes` が 0 に戻る。`n==2` の session クリアでは
   戻らない（3 到達性の担保）。
5. `resume_transcript_limit_bytes = 0` でゲートが無効、既存挙動のまま。
6. 既存テスト（`turn_engine_test.rs` / `pr_reviewer_test.rs` /
   `scheduler_test.rs` / `resume_test.rs`）が全通し。

## 変わらないもの（意図どおり）

- 完了コントラクト（result.json による成否裁定）は不変。pane tail は診断専用。
- ラベル2軸モデル・escalation の宛先（`escalation.rs`）は不変。栓3は既存の
  `NeedsHuman → flavor.escalate` に相乗りするだけ。
- 正常な resume（会話が生きている・transcript が閾値内）の挙動は不変。

## スコープ外（将来の話）

- session rotate / transcript サイズの stats 化（設計書 §5、測定 issue）。
- tmux での agent 不在検出の高精度化（本 issue は herdr-native を主とし、tmux は
  best-effort。事故現場は herdr）。
- P2〜P6（冪等 escalation・anchor 照合・impl_fixer 等）は各々別 issue。
