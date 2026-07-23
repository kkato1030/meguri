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
- **session_root は lane の pinned profile から解決する（f4）。** 現行の
  `session_root(&deps.config.agent)` は常に既定 agent を見るため、named profile
  が独自の `session_dir` を持つと transcript を見失い、oversize が gate を
  すり抜ける。**サイズ判定を握るゲートが唯一の権威**であり、run 内にいて
  `lane.profile` を持つので `session_root(&lane.profile)` で解決する。同じ理由で
  `record_agent_session` の session 探索（`flow.rs:2069`）も lane profile で
  揃える（run はターン開始時に profile を pin 済み）。
- **reaper の session 再スキャンは best-effort に格下げし、ゲートが backstop に
  なる（f4）。** reaper（`reaper.rs:498`）は run の外で動くため lane profile を
  持たず、`deps.config.agent` の既定 root しか使えない。ここは3点で安全化する:
  1. reaper の再スキャンは「pane 行にまだ session id が無い」ときの最後の網に
     限る。turn パス（`record_agent_session`、上で lane profile に是正済み）が
     毎ターン正しい root で id を保存するので、通常 reaper の推測は使われない。
  2. root がプロファイル差で外れても `latest_session_id` が `None` を返すだけで、
     既存の id は**上書きしない**（`if let Some(session)` ガード。`reaper.rs:507`）。
     誤った id を掴むのではなく「保存しない」に倒れる。
  3. **たとえ誤った・古い id が行に載っても、resume 時にゲートが `lane.profile`
     の正しい root でサイズを測り直す**ので、oversize は resume に至らない。
     つまり id 取得経路の解決ミスはゲートが必ず捕まえる — これが「共有契約」の
     実体（id 取得は best-effort、サイズ裁定は lane profile のゲートが一手に持つ）。
- **transcript を特定できない場合は fail-open。** custom CLI で jsonl レイアウトが
  違う・ファイルが無い等で `transcript_len()` が `None` のときは resume を止めない
  （既存挙動を維持）。取りこぼした死にセッションは栓3の strike backstop が
  境界を張る。この分岐は
  `pane.resume_gate_skipped { reason: "transcript_not_found" }` で可観測にする。
- ゲートは resume を試みる直前（`ensure_pane` の session_id 取得後、
  `spawn_agent_pane(..., Some(id))` の前）。超過なら **pane を kill/reclaim して
  から** saved session を消し（順序は §栓2「adopt ゲート」と同じ理由。live pane を
  残すと次 dispatch が adopt する）、
  `agent_session.cleared { reason: "transcript_oversize", bytes, limit }` を
  emit して fresh spawn へ落ちる。
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
- **adopt ゲート（f2）。** `ensure_pane` の live-pane 採用分岐は現状
  `pane_alive` だけで adopt し trigger を送る。素のシェルに落ちた pane も
  「生きている」ので adopt され、シェルへ trigger を打ち込む — 栓2 の nudge 対策を
  すり抜ける同じ穴が adopt 側にある。よって adopt する前に `agent_present` を見て、
  `Some(false)` なら adopt せず `release_pane`（kill + reclaim）して resume/fresh
  経路へ落とす。`Some(true)`/`None` は従来どおり adopt。
- **kill してから session を消す。** `PaneDied`/agent_absent で session を捨てる際、
  session id を消すだけでは `mux_pane_id` が残り次 dispatch が同じ pane を adopt
  する。`release_pane`（kill + `mark_pane_reclaimed` で mux_pane_id を消す）を先に
  呼び、その後 `save_pane_session(None)` で session を消す。順序が逆だと
  `release_pane` が transcript を再スキャンして死に session を再保存してしまう
  （`reaper.rs:498-510`）。これで次 dispatch は adopt 対象を持たず、resume する
  session も無いので確実に fresh spawn になる。
- **TOCTOU は許容し自己修復に委ねる（f3）。** `agent_present` 検査と `send_line` は
  別の mux 呼び出しなので、`Some(true)` の直後に agent が終了すると1回だけ nudge が
  シェルへ届きうる。これは明示的に許容する: (a) 窓は mux 1往復ぶんと短い、
  (b) シェルへ 1 行届いてもコマンドが `command not found` になるだけで害は無い、
  (c) 次 poll の `agent_present` 検査が `Some(false)` を返し1 interval 以内に
  `PaneDied` へ収束する。受け入れ基準2「不在が確定した pane に nudge を打たない」は
  送信直前の検査で決定的に満たす（FakeMux テストで担保）。mux 側の原子的
  send-if-present は herdr に該当プリミティブが無く、over-engineering として見送る。

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
- **AgentQuiet は `run_turn_in` の内部で正規化し、呼び出し側へは漏らさない（f1）。**
  `TurnEngine::await_completion` は `pub` で `Result<TurnOutcome>` を返すので、
  variant 追加は公開 enum の変更である。だが production の呼び出しは**全て**
  `flow::run_turn`（→ `run_turn_in`）を経由する（`cleaner.rs:615` /
  `flow.rs:2141,2318` / `pr_reviewer.rs:670` / `self_review.rs:1317` /
  `triage.rs:947`）。`await_completion` を直接叩くのはテストだけで、いずれも
  `matches!` なので非網羅 match で壊れない。よって:
  - `run_turn_in` は `await_completion` から `AgentQuiet` を受けたら、
    `record_agent_session` へ渡す前に共有ヘルパ `handle_agent_quiet` を呼んで
    strike を処理し、**呼び出し側へは既存の3 variant のみを返す**
    （strike<3 は `PaneDied` 相当、strike≥3 は `Err(NeedsHuman)` を伝播）。
    `run_turn` の戻り型 `(TurnOutcome, String)` は変えず、返す TurnOutcome が
    `AgentQuiet` になることは無い。
  - したがって上記6 呼び出し側の `match TurnOutcome { Completed / Stopped /
    PaneDied }` は**そのまま**でよい（新 arm 不要）。ただし `record_agent_session`
    と `run_turn_in` 内の `outcome_str` match（`flow.rs:1967`）には
    `AgentQuiet` arm を足す（同一関数内なので網羅漏れはここだけ）。
    `record_agent_session` の `AgentQuiet` arm は strike を触らない
    （strike 処理は `handle_agent_quiet` が所有）。
- 共有ヘルパ `handle_agent_quiet(deps, run, lane, tail) -> Result<TurnOutcome>`:
  - `n = bump_pane_quiet_strikes(...)`（+1 して新値を返す）
  - `n < 2` → session 温存のまま `Ok(TurnOutcome::PaneDied)`（次 dispatch で
    再 resume。一過性の沈黙に猶予を1回与える）
  - `n == 2` → **`release_pane`（kill + reclaim）してから** `save_pane_session(None)`
    で session を消し（§栓2 と同じ kill→clear 順）、
    `agent_session.cleared { reason: "quiet_loop" }` を emit → `Ok(PaneDied)`
    （次 dispatch は adopt 対象も resume 先も無く fresh spawn）
  - `n >= 3` → `Err(NeedsHuman(reason))`（reason に**サニタイズ済み** tail を同梱
    → 既存の `flavor.escalate` 経路がコメントを投稿。§栓4）
  - 返した `PaneDied` は呼び出し側で `Interrupted` にマップされ run が再 dispatch
    される（既存の PaneDied 経路）。ただし `record_agent_session` の
    `PaneDied if resumed` による session クリアと二重にならないよう、
    `handle_agent_quiet` は session を自分で管理し、`run_turn_in` は AgentQuiet を
    `record_agent_session` に渡さない（Completed/Stopped/PaneDied のみ渡す）。
- reset: `TurnOutcome::Completed` を `record_agent_session` で処理するとき
  `reset_pane_quiet_strikes(...)` で 0 に戻す。**session を消す `n==2` では
  reset しない** — reset すると fresh 後にまた quiet ったとき strike が 1 に
  戻り、3（needs-human）へ永遠に到達しないため。
- 定数: `QUIET_STRIKE_CLEAR = 2`、`QUIET_STRIKE_HUMAN = 3`。

### 栓4: agent_quiet の escalation に pane tail を同梱（受け入れ基準3）

栓3の `n >= 3` で `NeedsHuman` に載せる reason へ、pane 末尾 N=30 行を
埋める。読むのは診断のためで**成否裁定には使わない** — ADR 0026 の
「read するが裁定しない」と同じ立て付け。overview の「画面を読んで成否判定
しない」原則は破らない。

- **外部へ出す前にサニタイズする（f5）。** raw tail をそのまま Forge コメントへ
  流すと credential・PII・任意 Markdown・`@mention`/`#ref`・埋め込み ``` による
  fence 脱出・ANSI/制御文字を外部公開する。新ヘルパ
  `sanitize_pane_tail(lines, max_lines=30, max_bytes=4000) -> String` を通す:
  1. ANSI エスケープ列と制御文字を除去（印字可能 + 改行/タブのみ残す）。
  2. **トークン形の credential をマスクする。** 既知の高信号パターンを
     `‹redacted›` に置換する: `gh[pousr]_[A-Za-z0-9]{20,}`（GitHub token）・
     `sk-[A-Za-z0-9]{20,}`（OpenAI 系）・`AKIA[0-9A-Z]{16}`（AWS key id）・
     `xox[baprs]-[A-Za-z0-9-]{10,}`（Slack）・`(?i)(authorization|bearer|api[_-]?key|token|secret|password)\s*[:=]\s*\S+`（key=value 形）・
     40 桁以上連続の hex/base64 ラン。best-effort（全 credential の保証ではない）
     だが、tail に漏れやすい形は確実に落ちる。
  3. コードフェンスで囲む。GitHub はコードブロック内の `@`/`#` を通知・リンク化
     しないので、`@mention`/`#ref` の暴発はフェンスで無害化される。
  4. fence 脱出防止: 本文中の最長バッククォート連（``` ``` ``` 等）より1本長い
     フェンスで囲む（本文に4連があれば5連フェンス）。
  5. 行数 30・バイト 4000 で切り詰め、超過は `…(truncated)` を付す。
- **raw tail はローカル event 限定。** `turn.awaiting_human` / `turn.pane_died`
  等のイベント data には raw のまま載せてよい（DB 内・信頼境界内）。外部
  コメントへ出るのは常にサニタイズ後の文字列だけ。
- **多層防御。** ①トークンマスク ②event 限定の raw ③config
  `escalation.pane_tail = true`（既定）を `false` にすると tail 添付を完全に止め
  event 限定へ切り替え。完全な DLP は本 issue の範囲外（tail は diff と同じ
  meguri 自身の PR という信頼境界の診断抜粋）だが、上の3層で漏洩面を実務的に
  抑える。既定 true + マスクが受け入れ基準3を満たす。
- `flavor.escalate` はサニタイズ済み reason をそのままコメント本文にするので、
  追加の配線は要らない。

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
7. **`AgentQuiet` の正規化境界（f1）**: 公開 enum に variant を足すが、
   `run_turn_in` が唯一の消費点となり呼び出し側へは漏らさない。production は
   全て `run_turn` 経由（6 箇所を確認）、直接 `await_completion` を叩くのは
   `matches!` を使うテストのみ。よって6 呼び出し側の `match` は無改変で網羅を
   保ち、`AgentQuiet` arm が要るのは `run_turn_in` 内の2 match（`outcome_str` と
   `record_agent_session`）だけ。「公開境界で正規化する」を選んだ（全 call site
   に arm を撒くより変更が閉じる）。
8. **adopt ゲート（f2）**: `ensure_pane` の live-pane 採用を `pane_alive` だけで
   なく `agent_present != Some(false)` でもゲートする。素のシェル pane の
   trigger 打ち込みを塞ぐ。
9. **kill→clear の順序（f2）**: 死に session を捨てる全経路（transcript_oversize /
   quiet_loop / agent_absent）で `release_pane`（kill + reclaim）→
   `save_pane_session(None)` の順に統一。逆順だと `release_pane` が死に session を
   再保存する。次 dispatch が adopt も resume もできない状態にして fresh を保証。
10. **TOCTOU の扱い（f3）**: 送信直前検査 + 自己修復で許容。原子的 send は
    見送り。受け入れ基準2 は決定的検査で満たす。
11. **session_root の解決（f4）**: サイズ裁定を握る gate と `record_agent_session`
    は lane の pinned profile から `session_root` を解決する。reaper は run 外で
    profile を持たないので既定 root のまま best-effort（`None` は上書きしない）に
    格下げし、**resume 時のゲートが lane profile で測り直す**ことで id 取得経路の
    解決ミスを必ず捕まえる（これが3点共有契約の実体）。transcript 特定不能は
    fail-open + `pane.resume_gate_skipped`、栓3 が backstop。
12. **pane tail のサニタイズ（f5）**: 外部コメントへは `sanitize_pane_tail` を
    通した文字列のみ。多層防御 = ①トークン形 credential のマスク（`ghp_`/`sk-`/
    `AKIA`/`token=` 等 →`‹redacted›`）②ANSI/制御除去・フェンス脱出防止・
    行数/バイト上限 ③raw はローカル event 限定・`escalation.pane_tail=false` で
    添付停止。完全 DLP は範囲外（tail は diff と同信頼境界）だが漏洩面を実務的に
    抑える。

## 変更箇所

- `src/agent_session.rs` — `transcript_path()` / `transcript_len()` を追加。
- `src/config.rs` — `AgentProfile.resume_transcript_limit_bytes`（default 5MiB）、
  `escalation.pane_tail`（default true）。
- `src/mux/mod.rs` — `Multiplexer::agent_present()`（既定 `Ok(None)`）。
- `src/mux/herdr.rs` / `src/mux/tmux.rs` — `agent_present` 実装。
- `src/mux/fake.rs` — `agent_present` を setter で制御（`set_agent_present`）。
- `src/turn/mod.rs` — nudge 前の `agent_present` 検査 → `PaneDied`；
  quiet 上限で `TurnOutcome::AgentQuiet { tail }` を返す（park しない）。
  `sanitize_pane_tail()` はここか `src/engine/escalation.rs` に置く。
- `src/engine/flow.rs` — `ensure_pane` に adopt ゲート（`agent_present`）と
  transcript ゲート（lane profile で session_root 解決 + kill→clear）；
  `spawn_direct_process` に transcript ゲート；`handle_agent_quiet` ヘルパ
  （run_turn_in が唯一の消費点、呼び出し側へ AgentQuiet を漏らさない）；
  `record_agent_session` で Completed 時 reset + `AgentQuiet` arm（strike は
  触らない）；`run_turn_in` の `outcome_str` に `AgentQuiet` arm。
- `src/engine/reaper.rs` — `release_pane` を死に session クリア経路から再利用
  （呼び順の契約）。session 再スキャンは best-effort と明記（run 外なので既定
  root のまま、`None` は上書きしない。誤り id は resume 時のゲートが捕まえる、f4）。
- `src/store/panes.rs` — `PaneRecord.quiet_strikes`、`bump_/reset_` メソッド。
- `src/store/migrations/0017_pane_quiet_strikes.sql`（+ `mod.rs` の MIGRATIONS）。
- `docs/adr/00NN-session-health-converse-not-just-open.md` — 決定の記録
  （実装 PR に同梱。次の空き番号）。

## architecture impact

turn engine の契約に `TurnOutcome` の1 variant が増える（park→return の意味変更を
含む）。mux 抽象に1メソッド増える。この2つは公開契約だが、いずれも後方互換に
足せる: `agent_present` は既定 `Ok(None)`、`AgentQuiet` は `run_turn_in` が唯一の
消費点として吸収し呼び出し側へは既存3 variant のみを返す（f1）。resume 判定は
「開くか」から「会話できるか」へ拡張される
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
  `TurnOutcome::AgentQuiet` は公開 enum に増えるが `run_turn_in` で吸収され
  呼び出し側の型・DB・forge へは漏れないので、旧バイナリへ戻しても互換上の
  残骸は無い。破壊的 down-migration は不要（このリポジトリは forward-only）。
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
  - adopt ゲート（f2）: `agent_present==Some(false)` の live pane がある状態で
    `ensure_pane` を呼び、adopt されず（trigger が `sent_lines` に入らず）
    `release_pane` 経由で pane row の `mux_pane_id` が消え、次 spawn の argv が
    fresh（`--resume` 無し）であることを、**spawn 済み argv と pane row 両方**で
    検証（f2 の指摘どおり）。
  - 栓3: 同一 lane で quiet を3回起こし、1回目 Interrupted / 2回目
    `agent_session.cleared(quiet_loop)`＋`mux_pane_id` 消去＋session 消去 /
    3回目 needs-human＋コメントに（サニタイズ済み）tail、を検証。成功 turn で
    strike が 0 に戻り、`n==2` の clear では戻らないことも。
  - 栓1: 閾値超の transcript を seed し、resume されず fresh spawn（`--resume` を
    含まない spawn コマンド）＋ pane row が reclaim 済みになること。
    `resume_transcript_limit_bytes=0` でゲート無効も1本。named profile の
    `session_dir` を設定し、その配下の transcript でゲートが効くこと（f4）。
    transcript 不在で fail-open（resume 続行 + `pane.resume_gate_skipped`）も1本。
  - ゲート backstop（f4）: pane 行に oversize な session id が載った状態で
    resume を試み、`lane.profile` の root でサイズが測り直されて resume されず
    fresh に落ちること（id 取得経路の解決ミスをゲートが捕まえる、を実証）。
  - サニタイズ（f5）: 埋め込み ``` ``` ```・ANSI 列・`@here`・巨大行に加え、
    `ghp_`／`sk-`／`AKIA…`／`token=…` を含む tail を `sanitize_pane_tail` に
    通し、出力が fence 脱出しない／制御文字が無い／バイト上限内／`@`・`#` が
    コードブロックに閉じ込められ／**トークン形が `‹redacted›` にマスクされる**
    ことを検証。
- **統合（実 tmux + `tests/fixtures/fake_agent.sh`）**: 受け入れ基準1。
  fake_agent が resume 時に result を書かず沈黙 → 閾値超 transcript を事前配置
  →（人手なしに）fresh spawn が result を書いて復帰することを通しで確認。
  新規 `tests/resume_test.rs` に追記、または `tests/turn_engine_test.rs` を拡張。

## 受け入れ基準

1. 400 恒久ループを fixture 化した統合テストで、人手なしに fresh spawn へ
   復帰する（栓1 が初回 resume を止める／栓3 が2回目で session を捨てる）。
2. agent 不在 pane（`agent_present == Some(false)`）に nudge が打ち込まれない。
   加えて、素のシェル pane が adopt されず（trigger が届かず）次 dispatch の
   argv が fresh になる（f2）。
3. agent_quiet の escalation コメント（strike 3）に pane tail が含まれ、その
   tail は `sanitize_pane_tail` を通っている（fence 脱出・制御文字・上限超が無く、
   トークン形 credential が `‹redacted›` にマスクされる、f5）。
4. 成功 turn 完了で `quiet_strikes` が 0 に戻る。`n==2` の session クリアでは
   戻らない（3 到達性の担保）。session を捨てる全経路で pane row の
   `mux_pane_id` が消える（次 dispatch が adopt しない、f2）。
5. `resume_transcript_limit_bytes = 0` でゲートが無効、既存挙動のまま。named
   profile の `session_dir` 配下の transcript でもゲートが効く（f4）。
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
