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
- **判定**: `agent_session::transcript_len(session_root, worktree, session_id)`(新設。
  session id はそのまま transcript ファイル名なので `…/projects/<munged>/<id>.jsonl` の
  `metadata().len()` を返すだけ)が閾値を超えたら resume しない。
- **決定 D1 — 閾値の置き場所**: `AgentProfile` に `max_resume_transcript_bytes: u64`
  (`#[serde(default = …)]`、既定 `5 * 1024 * 1024`)。context window はプロファイル固有の
  性質(gpt と claude で違う)なのでプロファイル単位。`0` は「無効(サイズで弾かない)」。
  → **代替**: グローバル `[limits]` 1本。棄却 —— プロファイル毎に window が違うのに単一閾値では
  gpt に合わせると claude が過剰に fresh 化する。
- **超過時の挙動**: resume せず、保存 session id を落とし、`pane.resume_skipped`
  `{reason:"transcript_too_large", bytes, limit}` を emit、fresh spawn + full re-injection に
  フォールスルー(既存の「resume 失敗→full 再注入」経路をそのまま使う)。**quiet カウンタには
  触れない**(G2 とは独立の別トリガ)。

### G2. quiet ループの有界化

- **決定 D2 — 無応答を終端 outcome にする**: `TurnOutcome` に
  `AgentQuiet { tail: Vec<String> }` を追加。`await_completion` は nudge を撃ち尽くした時点で
  **今までの無限 park をやめ**、pane 末尾を読んで `AgentQuiet` を返す。
  → **代替**: park のまま crash recovery の再駆動に任せて回数だけ数える。棄却 —— park は
  終端しないので再駆動が daemon 再起動頼みになり、13時間ループの構造がそのまま残る。
- **決定 D3 — カウンタの置き場所と意味**: `panes` 行に `quiet_falls INTEGER NOT NULL
  DEFAULT 0`。session をまたいで積む(quiet_loop で session を捨ててもリセットしない)。
  healthy な `Completed` turn で 0 に戻す。
- **決定 D4 — 遷移を共有 seam に集約**: 分岐は `run_turn_in` の `record_agent_session` 直後に
  1か所だけ置く(全 lane が `run_turn`/`run_review_turn`/`run_parallel_review_turn` 経由で
  ここを通る)。閾値は名前付き定数 `QUIET_ROTATE_AT = 2` / `QUIET_ESCALATE_AT = 3`。
  - `Completed` → `quiet_falls = 0`。
  - `AgentQuiet` → `quiet_falls` を N に加算し:
    - **N < 2**: session を保持したまま `TurnOutcome::PaneDied` に翻訳して返す
      (呼び出し側の既存 PaneDied 経路が Interrupted → scheduler が再 dispatch → **同一 session を
      1回だけ resume 再試行**)。`turn.agent_quiet {fall:N, action:"retry"}` を emit。
    - **N == 2**: session を破棄(`agent_session.cleared {reason:"quiet_loop"}`)して `PaneDied` に
      翻訳(再 dispatch で session id が無いので **fresh spawn + full 再注入**)。
      `turn.agent_quiet {fall:N, action:"rotate"}` を emit。
    - **N >= 3**: `turn.awaiting_human {reason:"agent_quiet", fall:N, pane_tail:[…]}` を emit
      してから **`Err(NeedsHuman(<説明 + tail ブロック>))`** を返す。`run_flow` の Err 経路が
      `flavor.escalate` を呼び、needs-human ラベル + コメントが付く(G4)。
  → **なぜ Err(NeedsHuman) に集約するか**: 全呼び出し側は `run_turn(...).await?` で伝播するので、
    escalate の呼び出しを各 lane に足さずに1か所で needs-human に落とせる。PaneDied 側も
    既存の match arm を再利用でき、call site を増やさない。
- **direct mode**: nudge が無く AgentQuiet は起きない。resultless exit → PaneDied →
  `record_agent_session` の「PaneDied if resumed で session を落とす」既存経路が既にループを断つ。
  よって G2 は pane mode 専用。direct mode は G1 の恩恵だけ受ける。

### G3. agent の在否ゲート

- **決定 D5 — 在否は mux の一級能力**: `Multiplexer` に
  `async fn agent_present(&self, pane: &PaneId) -> MuxResult<bool>` を追加。既定実装は
  `Ok(true)`(在否を判別できない mux は挙動不変)。
  - **herdr**: native。pane に agent integration が登録されているか(agent_status の有無)で答える。
  - **tmux**: `#{pane_current_command}` を見て、shell 名(`zsh`/`bash`/`sh`/`fish` …)なら不在。
  - **FakeMux**: テスト用に設定可能なフラグ。
  → **代替**: `AgentState::Unknown` を「不在」とみなす。棄却 —— Unknown は tmux で生きた agent でも
    起きる(settle 前・非ブロック UI)ので在否に流用すると誤検出する(ADR 0028)。
- **呼ぶ場所は2か所**:
  - **(a) adopt ゲート**: `ensure_pane` が生きた pane を adopt する条件を
    `pane_alive && agent_present` にする。生きているが agent 不在なら adopt せず、
    `release_pane`(session 保存 + kill + reclaim)してから respawn 経路(G1 のサイズゲート適用)へ。
    `pane.agent_absent {lane}` を emit。→ **最初の trigger を素のシェルに打ち込む事故**を断つ。
  - **(b) nudge 直前**: `await_completion` の stagnation 分岐(`activity_clock >= idle_grace` で
    nudge を撃つ直前)で `agent_present` を確認し、不在なら **nudge せず** `AgentQuiet` を返す
    (G2 の機械に載る)。`turn.agent_absent {turn_id}` を emit。→ **turn 途中でシェルへ nudge**を断つ。
  - boot 中の誤検出は idle_grace(既定90秒)経過後にしか (b) を評価しないことで避ける。

### G4. 診断同梱(read するが裁定しない)

- **決定 D6 — pane tail を N=25 行**(`app.rs` の既存 `read_tail(&pane, 25)` に合わせる)。
  G2 の N>=3 escalation で、tail を `turn.awaiting_human` イベントの `pane_tail` に構造化して載せ、
  かつ `NeedsHuman` の理由文へフェンス済みブロックとして畳んでコメントに出す。
- **不変条件(overview / ADR 0025・0026)**: tail は人間の診断のための read であり、成否裁定・
  finding 生成には一切使わない。パースしない。整形して人間向けに出すだけ。

## 触るファイル

- `src/agent_session.rs` — `transcript_len`(または `transcript_path`)を追加(G1)。
- `src/config.rs` — `AgentProfile::max_resume_transcript_bytes` + 既定値関数(G1/D1)。
- `src/mux/mod.rs` — `Multiplexer::agent_present`(既定 `Ok(true)`)追加(G3/D5)。
- `src/mux/herdr.rs` / `src/mux/tmux.rs` / `src/mux/fake.rs` — `agent_present` 実装(G3)。
- `src/turn/mod.rs` — `TurnOutcome::AgentQuiet { tail }`、`await_completion` の nudge 撃ち尽くし時
  返却 + 在否チェック(G2/D2・G3(b))。`await_completion_direct` は不変。
- `src/turn/prompts.rs` — needs-human 理由に tail ブロックを畳むヘルパ(G4)。
- `src/engine/flow.rs` — `ensure_pane` の adopt ゲート(G3(a))、`ensure_pane` /
  `spawn_direct_process` の resume 前サイズゲート(G1)、`run_turn_in` の quiet 遷移集約(G2/D4)、
  `record_agent_session` は session-id 同期に専念(AgentQuiet は `_ => {}` で素通し)。
- `src/store/panes.rs` + `src/store/migrations/0017_pane_quiet_falls.sql` +
  `src/store/mod.rs`(migration 登録)— `quiet_falls` 列と bump/reset メソッド(G2/D3)。
- `tests/fixtures/fake_agent.sh` — 「resume 時に恒久 400 を出して結果を書かない」モードと
  「exit して素のシェルに落ちる」モードを追加(下記テスト strategy)。
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

- 新イベント: `pane.resume_skipped` / `pane.agent_absent` / `turn.agent_absent` /
  `turn.agent_quiet`(action=retry|rotate)。既存: `agent_session.cleared`(reason=quiet_loop)/
  `turn.awaiting_human`(reason=agent_quiet, pane_tail 付き)/ `escalation.raised`。
- これらから「session rotate 回数」「quiet_falls 分布」が events テーブルだけで出せる。
  設計書 §5 の `meguri stats review` への集計面追加は **本 issue のスコープ外**(計測は
  ADR 0026 の枠で別途)。ここではイベントを出すところまでを担保する。

## Test strategy

- **unit**:
  - `agent_session::transcript_len` のサイズ算出(存在しない id は None)。
  - `run_turn_in` の quiet 遷移(retry→rotate→escalate、Completed でリセット)を FakeMux で。
  - tmux `agent_present` の `pane_current_command` 分類(shell 名↔agent コマンド)。
  - migration の冪等性(既存 `migrations_apply_and_are_idempotent` に追随)。
- **統合(実 tmux + `fake_agent.sh`)**:
  - **受け入れ1**: fake_agent を「resume されたら恒久 400 を表示し結果を書かない」に。
    小さい閾値を設定して G1(または G2 の rotate)で fresh spawn に落ち、needs-human なしで
    success に到達することを、有界ターン内で確認(#222 の fixture 化)。
  - **受け入れ2**: fake_agent を「exit して素のシェルに落ちる」に。FakeMux/実 tmux の
    `sent_lines`(または pane 内容)に nudge 行が入らないこと、pane が release + respawn される
    ことを確認。
  - **受け入れ3**: quiet を3回起こし、needs-human コメントに tail 行が含まれることを FakeForge の
    記録に対して assert。
- FakeMux 拡張: `set_agent_present`(不在)と tail シード、不在時 `send_line` が呼ばれないこと。

## スコープ外(この issue では触らない)

- 設計書の P2〜P6.7(冪等 escalation・anchor 照合・impl fixer・重複デリバリ・sweep 可観測化・
  stop の claim 掃除)。それぞれ別 issue。
- `meguri stats review` への rotate 回数の集計面(計測は ADR 0026 の枠で別途)。
