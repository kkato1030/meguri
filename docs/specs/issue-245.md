# spec: issue #245 — 復旧不能セッションの resume ループを自動で断つ

> 使い捨ての足場。設計判断は ADR 0028 に、実装で消える。

## 目的

context 100%(恒久 400)・agent 不在 pane(素のシェル)といった復旧不能セッションへの
resume ループを、人手なしで断つ。設計の骨子は ADR 0028。この spec は受け入れ基準・触る
ファイル・決めた選択肢・移行/後退・テスト戦略を並べる。

## spec 深度の理由(design tier + migration 必須)

永続状態(pane 行の新カラム)・config スキーマ・`TurnOutcome`/`AgentState` という
内部契約に触れ、かつ**生きている agent セッションを自動で kill/ローテートする**という
不可逆な運用リスクを持つ。veto ルールにより migration & rollback を必須節として書く。

## 受け入れ基準(issue より)

1. 恒久 400 ループを fixture 化した統合テストで、人手なしに fresh spawn へ復帰する。
2. agent 不在 pane(素のシェル)に nudge が打ち込まれない。
3. agent_quiet の needs-human エスカレーションコメントに pane tail が含まれる。

## 決めた選択肢(A/B を先に潰す)

- **D1 閾値の持ち方**: グローバル既定 `[limits] resume_transcript_limit_bytes`(既定 5MiB)
  + プロファイル毎の任意上書き `AgentProfile.transcript_limit_bytes: Option<u64>`。
  実効値 = プロファイル override ?? グローバル既定。`0` はゲート無効(逃げ道)。
  → プロファイル専用にしない: 大半のプロジェクトは1本の既定で足り、gpt 等だけ絞れれば良い。
- **D2 ストライク閾値**: モジュール定数 `QUIET_CLEAR_AT = 2` / `QUIET_HUMAN_AT = 3`(issue の
  数字そのまま)。config には出さない — ノブを増やさない。将来必要なら limits へ昇格。
- **D3 `AgentQuiet` の可視範囲(f1 訂正)**: `TurnOutcome` は `pub` で `await_completion` も
  `pub`、統合テストが直接呼ぶ。よって「呼び出し側を変えない」は成立しない — 変異体を足せば
  Rust の網羅 match が全 call site で割れる。正規化境界を `run_turn_in` に置いた上で、**壊れる
  match を全て列挙して直す**:
  - `await_completion` は `AgentQuiet(Vec<String>)`(pane tail 同梱)を返す。直接呼ぶテスト
    (`turn_engine_test.rs`)は新動作へ更新。
  - `run_turn_in`(正規化点): `AgentQuiet` を strike ハンドラへ。終端(strike≥3)は
    `Err(NeedsHuman)`。ローテートは `Ok((AgentQuiet, turn_id))` のまま返す(PaneDied へ
    詰め替えない)。`outcome_str` の match に `AgentQuiet => "agent_quiet"` を足す。
  - flow の各 loop call site(`execute` / `validate` / `self_review.rs` ×3 / `pr_reviewer.rs`
    / `cleaner.rs` / `triage.rs`)は、既存の `PaneDied` 腕に `AgentQuiet` を**合流**させる
    (`TurnOutcome::PaneDied | TurnOutcome::AgentQuiet => …Interrupted`)。制御フローは不変、
    ただし網羅性のため腕の**パターンは1語増える**。
  - `record_agent_session` の match は `AgentQuiet` を `_ => {}` で吸収(session 操作は
    `run_turn_in` の strike ハンドラ側が担う)。
  - `await_completion_direct` は `AgentQuiet` を**生成しない**(direct モードに nudge/quiet は
    無い)。返り値型は共有だが値は Completed/Stopped/PaneDied のみ。
- **D4 ローテート機構**: 新しいリトライループを足さない。既存の
  `Interrupted → redispatch_interrupted` に相乗りする(`PaneDied → StepFlow::Interrupted`)。
  再 dispatch 時、strike 2 で session がクリア済なら `ensure_pane` が fresh spawn する。
- **D5 ストライクの置き場**: `panes.quiet_strikes`(project × issue × lane、run をまたいで残る)。
  `Completed` ターンで 0 リセット。
- **D6 `Absent` の検出元と波及(f2 訂正)**: tmux = `pane_current_command` が既知シェル
  (`zsh`/`bash`/`sh`/`fish`/`-zsh` …)なら `Absent`(agent は `node`/`git`/`cargo` 等で出る
  のでシェルだけが該当)。herdr = native な「agent 不在」状態(判別不能なら従来どおり
  `Unknown`)。変異体追加で割れる match/契約を全列挙して直す:
  - `AgentState::as_str`: `Absent => "absent"`。
  - `turn/mod.rs await_completion` の `match state`(`Blocked`/`Working`/`Idle|Done|Unknown`):
    `Absent` 腕を足し、**nudge/quiet 時計を回さず即 `PaneDied`**。
  - `herdr.rs`: `map_agent_status` は native の不在ステータスがあれば `Absent` に写像
    (無ければ従来 `Unknown` のまま)。`agent_status_arg` は match を total に保つため
    `Absent => "unknown"`(herdr へ wait 目標としては渡さない)。
  - `wait_state` の targets: `Absent` は**待機対象にしない**(呼び出し側は従来どおり
    Working/Idle/Blocked/Done を待つ)。`Absent` はエンジンが導出するだけの状態。
  - テスト契約: `herdr.rs` の `agent_status_round_trips_through_wait_args` は `Absent` が
    `unknown` へ潰れるため round-trip の例外として更新。`fake.rs` は変更不要
    (`set_state(pane, Absent)` で駆動)。mux テスト(tmux/herdr)に `Absent` ケース追加。
  - `Absent` は `await_completion` と `resumed_pane_survives` の両方で「生存せず」扱い。
- **D7 人間を呼ぶタイミング**: 3回目の strike だけ。1・2回目は `turn.agent_quiet`(情報のみ、
  ページしない)。能動的な救済は `Takeover` 経路(quiet 判定より前に honor 済み)。
- **D8 診断 tail**: 末尾 `QUIET_TAIL_LINES = 40` 行。read-only。制御には一切使わない。
- **D9 `agent_session.cleared` の reason**: `transcript_oversize` / `quiet_loop`(既存の
  「resumed executor died…」に加える)。
- **D10 再 dispatch を本当に fresh spawn へ進める(f3 修正・最重要)**: `ensure_pane` は
  `pane_alive` だけで live pane を adopt するため、strike 2 で session id を消しても
  `mux_pane_id` が残れば同じ pane を adopt し、次の trigger が素のシェルへ入る。二重で塞ぐ:
  1. **teardown(主機構)**: quiet ローテート(strike≥2)と `Absent → PaneDied` の両経路で、
     session id を `None` にした上で **pane を kill + `mark_pane_reclaimed`**
     (`reaper::release_pane` は生存セッションを保存しに行くので、ここでは session=None を
     確定させてから解放する)。これで次 dispatch は `mux_pane_id` 無し → resume か fresh
     spawn へ進む。
  2. **adopt ゲート(防御)**: `ensure_pane` の adopt 分岐に「live pane の `agent_state` が
     `Absent` なら adopt せず release して再 spawn へ落とす」条件を足す。teardown が漏れても
     素のシェルを adopt しない。
  - strike 1(session 保持)の再試行は、pane が生きていれば従来どおり adopt して同一
    session を resume。context 一過性ならこれで回復、駄目なら strike 2 で teardown。
- **D11 `session_root` は pinned lane profile から解決する(f4 修正)**: transcript サイズ
  ゲートの `session_root` は、`deps.config.agent` 固定ではなく **その lane の pinned profile**
  (`lane.profile`)から `agent_session::session_root(&lane.profile)` で引く — named profile の
  `session_dir` や custom CLI の transcript を見失わないため。あわせて既存の latent 不整合も
  正す: `record_agent_session`(現 `session_root(&deps.config.agent)`)へ lane profile を
  スレッドし、reaper のセッション退避も run の pinned profile から root を引く。**未対応
  レイアウトの fallback**: transcript を特定できない時はゲートを**素通り**(resume を止めない)
  にし、`transcript.locate_failed` を emit。oversize の取りこぼしは quiet-strike 経路が backstop。
- **D12 pane tail を出す前にサニタイズする(f5 修正・security)**: 生の tail は**ローカル
  イベント限定**(`turn.agent_quiet` は sqlite に残るだけ)。Forge コメント/通知へ出す時は
  `sanitize_pane_tail(lines) -> String` を通す:
  1. ANSI エスケープ・制御文字を除去(端末エスケープ注入を断つ)、
  2. バイト上限で切り詰め(既定 4KiB、露出量を bound)、
  3. ` ``` ` フェンスで囲み Markdown を中和(任意 Markdown/メンション注入を断つ)。
  秘密の網羅 redaction は原理的に困難なので範囲外とし、代わりに「フェンス+ANSI除去+バイト
  上限」で注入と露出量を抑える方針を採る。ADR 0028 §4 にこの決定を反映済み。受け入れテストに
  「ANSI・バックティック・制御文字を含む tail がコメント上でサニタイズされる」を追加。

## 触るファイル

- `src/turn/mod.rs`
  - `TurnOutcome::AgentQuiet(Vec<String>)` を追加(pane tail 同梱)。`outcome_str` の match に
    `AgentQuiet => "agent_quiet"` を足す。
  - `await_completion`: nudge 撃ち尽くしで `park` せず、pane tail を読んで `turn.agent_quiet`
    を emit し `AgentQuiet` を返す。`match state` に `Absent` 腕を足し、nudge/quiet 時計を
    回さず即 `PaneDied`(event `turn.pane_died` に `reason: "agent_absent"`)。
  - `await_completion_direct`: `AgentQuiet` は生成しない(変更は無いが網羅性の確認対象)。
- `src/mux/mod.rs`: `AgentState::Absent` + `as_str` に `"absent"`。
- `src/mux/tmux.rs`: `agent_state` で `pane_current_command`(`display-message #{pane_current_command}`)
  を見て素のシェルを `Absent` に。
- `src/mux/herdr.rs`: `map_agent_status` に不在写像(不明なら `Unknown`)、`agent_status_arg` に
  `Absent => "unknown"`、round-trip テストを更新。
- `src/mux/fake.rs`: 変更なし(`set_state` が任意の `AgentState` を取れる)。テストは
  `set_state(pane, Absent)` で駆動。
- `src/agent_session.rs`: `transcript_len(session_root, worktree, session_id) -> Option<u64>`。
- `src/config.rs`: `LimitsConfig.resume_transcript_limit_bytes`(既定 5MiB)/
  `AgentProfile.transcript_limit_bytes: Option<u64>`(serde default、後方互換)。
- `src/engine/flow.rs`
  - `ensure_pane`: (a) resume 前に transcript サイズゲート — root は **lane profile** から
    解決(D11)、超過なら session 破棄(`agent_session.cleared: transcript_oversize`)して
    fresh spawn。(b) adopt 分岐に `Absent` ゲート(D10-2)。
  - `spawn_direct_process`: 同じゲート(root は lane profile から解決)。
  - `resumed_pane_survives`: `Absent` を「生存せず」に。
  - `run_turn_in`: `AgentQuiet` を strike ハンドラへ。bump →(strike≥2 で session=None +
    **pane teardown**: kill + `mark_pane_reclaimed`, D10-1)→ strike≥3 は
    `Err(NeedsHuman(sanitize_pane_tail(tail) 同梱))`、それ未満は `Ok((AgentQuiet, id))`。
  - `record_agent_session`: `Completed` で `quiet_strikes` を 0 リセット。session_root は
    lane profile から(D11、既存 latent 不整合の是正)。`AgentQuiet` 腕は `_ => {}`。
  - `Absent → PaneDied` 経路でも pane teardown(kill + `mark_pane_reclaimed`)して再 adopt を防ぐ。
- `src/engine/escalation.rs`(または flow の needs-human 経路): tail を埋め込む前に
  `sanitize_pane_tail` を通す(D12)。ローカルイベントには生 tail、Forge/通知にはサニタイズ版。
- `src/engine/reaper.rs`: セッション退避の `session_root` を run の pinned profile から引く(D11)。
- `src/store/panes.rs`: `PaneRecord.quiet_strikes` / `bump_quiet_strikes`(新値を返す)/
  `reset_quiet_strikes`。
- `src/store/migrations/0017_pane_quiet_strikes.sql` を新規 + `src/store/mod.rs` に登録。
- fixture: `tests/fixtures/fake_agent.sh` に「resume で恒久沈黙(result を書かない・画面も
  動かさない)」モードを足し、context 100% の 400 を再現。
- テスト(下記「テスト戦略」)。

## 制御フロー(実装後)

```
resume 要求(ensure_pane / spawn_direct_process)
  ├ adopt: live pane が Absent → release して再 spawn へ(D10-2)
  └ transcript_len(root=lane profile) > 実効閾値 ?
        → session 破棄(cleared: transcript_oversize) → fresh spawn
await_completion(pane)
  ├ AgentState::Absent            → PaneDied(reason: agent_absent)  ※nudge しない
  │                                  + pane teardown(kill + mark_pane_reclaimed)
  ├ result あり                    → Completed(→ quiet_strikes = 0)
  └ idle_grace 超過 × nudge_limit → AgentQuiet(tail を turn.agent_quiet に添付)
run_turn_in が AgentQuiet を受けたら:
  strikes = bump_quiet_strikes(lane)
  strikes >= 2 → session=None + pane teardown(cleared: quiet_loop) ← 次 dispatch で fresh spawn
  strikes >= 3 → Err(NeedsHuman(sanitize_pane_tail 同梱))          ← escalate_task が needs-human
  それ未満     → Ok((AgentQuiet, id))  → call site は PaneDied と合流し Interrupted → redispatch
```

`Absent`(resumed だった場合)は `record_agent_session` が session を破棄し、加えて pane を
teardown するので、`mux_pane_id` の残留で素のシェルを再 adopt することはない(f3)。

## Architecture impact / alternatives

- **代替1: quiet で park し続け、別 sweep が古い awaiting_human を回収する。** 却下 —
  回収の遅延がそのままループ時間になる。turn を打ち切って redispatch に載せる方が単純で速い。
- **代替2: `AgentQuiet` の終端判定を各 call site に持たせる。** 却下 — strike 数は pane 行に
  あり call site は知らない。正規化点は `run_turn_in` に一本化し、call site は `AgentQuiet` を
  `PaneDied` と同じ `Interrupted` 腕へ合流させるだけにする(D3。網羅性のため腕は1語増える —
  「不変」ではない、が制御は不変)。
- **代替3: `Absent` を足さず「素のシェルは Idle」のまま tail 正規表現で判定。** 却下 —
  画面スクレイプでの成否裁定に踏み込み overview 原則に反する。プロセス在否は mux の責務(ADR 0028)。
- **代替4: transcript サイズでなくターン数/トークン推定でゲート。** 却下 — transcript バイト数は
  agent 非依存で観測でき(既存の session ファイル走査を流用)、代理指標として十分。

## Migration & rollback(必須)

- **前進**: `0017_pane_quiet_strikes` が `panes` に `quiet_strikes INTEGER NOT NULL DEFAULT 0`
  を追加(`ALTER TABLE ADD COLUMN`、既存行はデフォルト 0)。config の2フィールドは serde
  default 付きで加える。データ移行は不要。
- **後退**: 新カラムは実行時に導出される揮発状態で、意味的に捨てて良い。旧バイナリの
  `pane_from_row` は列名指定で読むため `quiet_strikes` を無視し、旧バイナリの INSERT は
  デフォルト 0 に任せる — 前後どちらでも動く。`TurnOutcome::AgentQuiet` / `AgentState::Absent`
  はメモリ内のみ(DB へは `finish_turn` が文字列 `"agent_quiet"` を書くだけで、旧コードは
  ただの text として保持)。config 追加フィールドは旧バイナリが serde default で無視。
- **運用リスクと緩和**: 自動ローテートは生きたセッションを kill しうる。緩和 = 高々3回で
  頭打ち / `Takeover` 中は quiet 判定を回さない / transcript ゲートは 5MiB 超のみ発火 /
  閾値 `0` で完全無効化。ロールバックは旧バイナリへ戻すだけ(スキーマ後方互換)。

## Observability

- events: `turn.agent_quiet {strikes, tail}`(生 tail はローカル sqlite 限定、f5)/
  `agent_session.cleared {reason}` / `turn.pane_died {reason: "agent_absent"}` /
  `transcript.locate_failed`(f4)/ 3回目の `escalation.raised`(既存経路)。
- Forge コメント・通知に出るのは `sanitize_pane_tail` を通した版だけ(f5)。
- `meguri stats` / `doctor` への session rotate 回数の露出は設計書 §5 の宿題として
  別 issue に回す(本 issue の受け入れ基準には含めない)。

## テスト戦略

- **統合(受け入れ基準1)**: 実 tmux + 拡張 `fake_agent.sh` の「resume で恒久沈黙」モードで、
  ≤3回のうちに fresh spawn(`--resume` なし・full prompt)へ人手なしで復帰し、3回目で
  needs-human になることを検証。
- **flow(FakeMux、resume_test.rs 系)**:
  - transcript サイズ超過 → resume せず fresh spawn(`agent_session.cleared: transcript_oversize`)。
    named profile の `session_dir` を使う env で、`deps.config.agent` でなく lane profile の
    root で oversize を検出できること(f4)。特定不能レイアウトはゲート素通り +
    `transcript.locate_failed`(f4 fallback)。
  - quiet ストライク: 1→再試行 / 2→`quiet_loop` で session クリア **かつ pane が
    teardown(`mark_pane_reclaimed`)されて次 spawn が `--resume` なし** になること(f3)/
    3→`NeedsHuman`(reason にサニタイズ済 tail)。
  - `Completed` で `quiet_strikes` が 0 に戻る。
  - **adopt ゲート(f3)**: `mux_pane_id` が生きていても `agent_state == Absent` なら adopt せず
    release → 再 spawn になること。
- **turn engine(turn_engine_test.rs)**:
  - 既存 `quiet_agent_gets_nudged_then_escalates` を新動作へ更新(park せず `AgentQuiet` を返す)。
  - `Absent` → nudge 0 で `PaneDied`(受け入れ基準2)。
- **mux(mux_tmux_test.rs / mux_herdr_test.rs)**: 素のシェルの pane が `agent_state == Absent`。
  herdr の `agent_status_arg`/round-trip テストを `Absent` 追加に合わせて更新(f2)。
- **サニタイズ(f5、単体 + 受け入れ)**: `sanitize_pane_tail` が ANSI・制御文字を除去し、
  バイト上限で切り詰め、フェンスで囲むこと。ANSI/バックティック/制御文字入りの tail が
  needs-human コメント上で無害化されること(受け入れ基準3 と両立)。
- **回帰**: `notify_test.rs` / `pr_reviewer_test.rs` / `turn_tmux_test.rs` の
  agent_quiet 期待値を新イベント名・新経路に合わせて更新。

## 実装後の後始末

この spec は実装完了時に削除する。ADR 0028 が「なぜ」を、コードが「どう」を持つ。
`quiet_strikes` の意味(issue × lane の連続 quiet、完了で 0)はコードのコメントに残す。
