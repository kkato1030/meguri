# 0029: resume の条件は「pane が開くか」ではなく「その会話がまだ生きているか」

- Status: accepted
- Date: 2026-07-23
- 関連: issue #245、`docs/design/needs-human-friction-and-delivery-speed.md` §3-A / §P1、
  ADR 0026(read するが裁定しない)

## 文脈

context window の小さいプロファイル(gpt 系)のセッションが context 100% に達すると、
以後の入力はすべて API 400 になる。`ensure_pane` は pane 行の `agent_session_id` を
無条件に `--resume` するため、nudge → agent_quiet → recover → 同一セッション resume
→ 400 のループが人手が入るまで続いた(実測 13 時間超、#222・#235 で再発)。また
agent プロセスが死んで pane が素の zsh に落ちても、pane 生存だけを見る検出は nudge を
シェルに打ち込み続けた。「pane が開く」ことと「その会話に話しかけて返事が返る」ことは
別物である。

## 決定

resume・adopt・quiet 処理のすべてで「会話が生きているか」を条件にし、生きていないと
判明したセッションは人手なしに捨てて fresh spawn に落とす。実装は独立した3つの栓:

1. **transcript サイズゲート**: resume 直前に lane の pinned profile の
   `session_root` から transcript サイズを測り、profile 毎の閾値
   (`resume_transcript_limit_bytes`、既定 5MiB、`0` で無効)を超えていたら
   pane を kill/reclaim → saved session を消して
   (`agent_session.cleared { reason: "transcript_oversize" }`)fresh spawn する。
   プロンプトは自己完結なので文脈再構築は不要。transcript を特定できないときは
   **fail-open**(resume 続行 + `pane.resume_gate_skipped`)— 取りこぼしは栓3が拾う。
   サイズ裁定はこのゲートが唯一の権威で、id 取得経路(reaper の再スキャン等)は
   best-effort のまま — 誤った root で拾われた id も resume 時に正しい root で
   測り直されるため。
2. **agent 不在の検出**: `Multiplexer::agent_present() -> Option<bool>` を新設
   (既定 `None` = 判定不能)。herdr は pane の前景プロセス一覧、tmux は
   `#{pane_current_command}` で「全部シェル = agent 不在」を判定する。
   turn engine は nudge 直前に、`ensure_pane` は live pane の adopt 直前に、
   `Some(false)` なら nudge/adopt せず pane を畳む。`None` は従来どおり進む
   (fail-open — 成否裁定は常に result file)。検査と送信の間の TOCTOU は許容:
   窓は mux 1往復で、外れても次 poll が収束させる。
3. **quiet strike ladder**: nudge を使い切った quiet は park(awaiting_human で
   永久待機)せず `TurnOutcome::AgentQuiet { tail }` として flow 層に返し、
   lane 毎の `panes.quiet_strikes` で数える。1回目 = 猶予(同一 session で再試行)、
   2回目 = session 破棄(`agent_session.cleared { reason: "quiet_loop" }` +
   pane kill/reclaim → 次は必ず fresh spawn)、3回目 = needs-human。
   カウンタは完了 turn だけがリセットする — 破棄時にリセットすると 3 に永遠に
   到達しない。`AgentQuiet` は `run_turn_in` が唯一の消費点で、各 loop には
   既存3 variant しか届かない(公開境界での正規化)。

needs-human escalation には pane 末尾 30 行を**サニタイズして**同梱する
(`sanitize_pane_tail`: ANSI/制御文字除去、トークン形 credential のマスク、
行数/バイト上限、fence 脱出不能なコードブロック化)。読むのは診断のためだけで、
成否裁定には使わない(ADR 0026 と同じ立て付け)。raw tail はローカル events 限定。
`limits.escalation_pane_tail = false` で同梱を止められる
(`[escalation]` はプロファイル escalation チェーンの flatten テーブルなので、
boolean を同居させられず `[limits]` に置いた)。

## 影響

- `panes` に `quiet_strikes` 列(migration 0017、追加列のみ・rollback は放置で可)。
- `Multiplexer` に既定実装付きメソッド1つ、`TurnOutcome` に variant 1つ
  (どちらも後方互換。variant は flow 内で吸収され呼び出し側の型に現れない)。
- 「turn engine は quiet で park しない」への意味変更: awaiting_human(agent_quiet)
  は strike 3 の needs-human に一本化された。runtime budget 超過の park は不変。
- 緊急時の無効化: 栓1は `resume_transcript_limit_bytes = 0`、栓2/3は
  `agent_present` が `None` の mux では自然に不活性(strike ladder は残る)。
- ADR 0023 の異種モデル路線で小さい context window のプロファイルが増えるほど
  このゲート群が前提になる。
