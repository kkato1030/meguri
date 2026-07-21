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
- **D3 `AgentQuiet` の可視範囲**: `TurnOutcome::AgentQuiet` は turn engine ↔ `run_turn_in` の
  内部限定。`run_turn_in` が `PaneDied`(ローテート)か `Err(NeedsHuman)`(終端)へ**変換**して
  返すので、`execute`/`validate`/`self_review`/`pr_reviewer`/`cleaner`/`triage` の6つの
  呼び出し側 match は**変えない**。
- **D4 ローテート機構**: 新しいリトライループを足さない。既存の
  `Interrupted → redispatch_interrupted` に相乗りする(`PaneDied → StepFlow::Interrupted`)。
  再 dispatch 時、strike 2 で session がクリア済なら `ensure_pane` が fresh spawn する。
- **D5 ストライクの置き場**: `panes.quiet_strikes`(project × issue × lane、run をまたいで残る)。
  `Completed` ターンで 0 リセット。
- **D6 `Absent` の検出元**: tmux = `pane_current_command` が既知シェル(`zsh`/`bash`/`sh`/`fish`
  /`-zsh` …)なら `Absent`(agent は `node`/`git`/`cargo` 等で出るのでシェルだけが該当)。
  herdr = native な「agent 不在」状態(判別不能なら従来どおり `Unknown`)。`Absent` は
  `await_completion` と `resumed_pane_survives` の両方で「生存せず」扱い。
- **D7 人間を呼ぶタイミング**: 3回目の strike だけ。1・2回目は `turn.agent_quiet`(情報のみ、
  ページしない)。能動的な救済は `Takeover` 経路(quiet 判定より前に honor 済み)。
- **D8 診断 tail**: 末尾 `QUIET_TAIL_LINES = 40` 行。read-only。制御には一切使わない。
- **D9 `agent_session.cleared` の reason**: `transcript_oversize` / `quiet_loop`(既存の
  「resumed executor died…」に加える)。

## 触るファイル

- `src/turn/mod.rs`
  - `TurnOutcome::AgentQuiet` を追加。
  - `await_completion`: nudge 撃ち尽くしで `park` せず、pane tail を読んで `turn.agent_quiet`
    を emit し `AgentQuiet` を返す。`AgentState::Absent` を見たら nudge/quiet 時計を回さず
    即 `PaneDied`(event `turn.pane_died` に `reason: "agent_absent"`)。
- `src/mux/mod.rs`: `AgentState::Absent`(+ `as_str`)。
- `src/mux/tmux.rs`: `agent_state` で `pane_current_command` を見て素のシェルを `Absent` に。
- `src/mux/herdr.rs`: native status → `Absent` の写像(不明なら `Unknown`)。
- `src/mux/fake.rs`: 変更なし(`set_state` が任意の `AgentState` を取れる)。テストは
  `set_state(pane, Absent)` で駆動。
- `src/agent_session.rs`: `transcript_len(session_root, worktree, session_id) -> Option<u64>`。
- `src/config.rs`: `LimitsConfig.resume_transcript_limit_bytes`(既定 5MiB)/
  `AgentProfile.transcript_limit_bytes: Option<u64>`(serde default、後方互換)。
- `src/engine/flow.rs`
  - `ensure_pane`: resume 前に transcript サイズゲート。超過なら session を破棄
    (`agent_session.cleared` reason `transcript_oversize`)して fresh spawn。
  - `spawn_direct_process`: 同じゲート(direct モードの self-reviewer 等)。
  - `resumed_pane_survives`: `Absent` を「生存せず」に。
  - `run_turn_in`: `AgentQuiet` を横取り → strike ハンドラ(bump / 2で clear / 3で
    needs-human)→ `PaneDied` か `Err(NeedsHuman(tail 同梱))` へ変換。
  - `record_agent_session`: `Completed` で `quiet_strikes` を 0 リセット。
- `src/store/panes.rs`: `PaneRecord.quiet_strikes` / `bump_quiet_strikes`(新値を返す)/
  `reset_quiet_strikes`。
- `src/store/migrations/0017_pane_quiet_strikes.sql` を新規 + `src/store/mod.rs` に登録。
- fixture: `tests/fixtures/fake_agent.sh` に「resume で恒久沈黙(result を書かない・画面も
  動かさない)」モードを足し、context 100% の 400 を再現。
- テスト(下記「テスト戦略」)。

## 制御フロー(実装後)

```
resume 要求(ensure_pane / spawn_direct_process)
  └ transcript_len > 実効閾値 ? → session 破棄(cleared: transcript_oversize) → fresh spawn
await_completion(pane)
  ├ AgentState::Absent            → PaneDied(reason: agent_absent)  ※nudge しない
  ├ result あり                    → Completed
  └ idle_grace 超過 × nudge_limit → AgentQuiet(tail を turn.agent_quiet に添付)
run_turn_in が AgentQuiet を受けたら:
  strikes = bump_quiet_strikes(lane)
  strikes >= 2 → session 破棄(cleared: quiet_loop)   ← 次の再 dispatch で fresh spawn
  strikes >= 3 → Err(NeedsHuman(tail 同梱))           ← escalate_task が needs-human + コメント
  それ以外     → PaneDied を返す(Interrupted → redispatch → 再試行)
Completed 時 → quiet_strikes = 0
```

Absent(resumed だった場合)の `PaneDied` は既存 `record_agent_session` が session を破棄する
ので、素のシェル落ちループも同経路で断てる。

## Architecture impact / alternatives

- **代替1: quiet で park し続け、別 sweep が古い awaiting_human を回収する。** 却下 —
  回収の遅延がそのままループ時間になる。turn を打ち切って redispatch に載せる方が単純で速い。
- **代替2: `AgentQuiet` を全ループの呼び出し側に見せる。** 却下 — 6箇所の match を触る
  churn とバグ面が増える。`run_turn_in` 内で `PaneDied`/`NeedsHuman` に畳めば呼び出し側は不変(D3)。
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

- events: `turn.agent_quiet {strikes, tail_lines, attach}` /
  `agent_session.cleared {reason}` / `turn.pane_died {reason: "agent_absent"}` /
  3回目の `escalation.raised`(既存経路)。
- `meguri stats` / `doctor` への session rotate 回数の露出は設計書 §5 の宿題として
  別 issue に回す(本 issue の受け入れ基準には含めない)。

## テスト戦略

- **統合(受け入れ基準1)**: 実 tmux + 拡張 `fake_agent.sh` の「resume で恒久沈黙」モードで、
  ≤3回のうちに fresh spawn(`--resume` なし・full prompt)へ人手なしで復帰し、3回目で
  needs-human になることを検証。
- **flow(FakeMux、resume_test.rs 系)**:
  - transcript サイズ超過 → resume せず fresh spawn(`agent_session.cleared: transcript_oversize`)。
  - quiet ストライク: 1→再試行 / 2→`quiet_loop` で session クリア / 3→`NeedsHuman`(reason に tail)。
  - `Completed` で `quiet_strikes` が 0 に戻る。
- **turn engine(turn_engine_test.rs)**:
  - 既存 `quiet_agent_gets_nudged_then_escalates` を新動作へ更新(park せず `AgentQuiet` を返す)。
  - `Absent` → nudge 0 で `PaneDied`(受け入れ基準2)。
- **mux(mux_tmux_test.rs)**: 素のシェルの pane が `agent_state == Absent`。
- **回帰**: `notify_test.rs` / `pr_reviewer_test.rs` / `turn_tmux_test.rs` の
  agent_quiet 期待値を新イベント名・新経路に合わせて更新。

## 実装後の後始末

この spec は実装完了時に削除する。ADR 0028 が「なぜ」を、コードが「どう」を持つ。
`quiet_strikes` の意味(issue × lane の連続 quiet、完了で 0)はコードのコメントに残す。
