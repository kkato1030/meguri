# issue-66 spec — routing (3/3): 詰まったら昇格 + explore_ratio

routing 1/3(#64)の静的な役割→プロファイル振り分けに、**難易度を推定せず**難易度へ
適応する 2 つの仕組みを足す。設計判断の理由は ADR 0012 に置いた——この spec は実装を
収束させるための足場で、着地時に消える。

## spec 深度の理由(design spec)

`runs` にスキーマ列を 1 つ足し(`routing_arm`)、`[routing]` という公開設定契約を拡張し、
`runs.agent_profile` の書き込み意味論(昇格で上書きされうる)を変える。加えて session
再注入・チェーンの出所・stats 帰属に本物の不確実性がある。よって **design spec**、
かつ永続状態に触れるので migration & rollback を必須セクションとして書く(veto ルール)。

## 何を作るか(2 つの仕組み)

### A. プロファイルエスカレーション「詰まったら昇格」

- **トリガー**: `flow::validate` の検証失敗ループで、fix ターンが 2 回目に入るとき
  (`cp.fix_turns_used` が 2 に達した時点、`validate_turns` を使い切る前)。役割に
  エスカレーションチェーンが定義され、まだ末尾でなければ昇格する。
- **動作**: `cp.escalation_level` を 1 つ上げ、`escalation_chain(role)[level]` を新
  プロファイルとして `runs.agent_profile` に上書き pin し直す。live な author ペインを
  retire、ペイン行の `agent_session_id` を **None にクリア**(モデルが変わるので resume
  不可)。次の fix ターンは新プロファイルで新規 spawn され、プロンプトには従来どおり
  検証失敗の内容 + 「モデルを上げて再挑戦」の一文が載る。
- **有限性**: チェーン末尾で更に失敗し `validate_turns` を超えたら、従来どおり
  needs-human(`NeedsHuman`)。無限昇格しない。

### B. explore_ratio(opt-in canary、既定 off)

- 初回プロファイル解決時(`resolve_run_profile` が pin する箇所)に、`explore_ratio > 0`
  なら issue 番号の**決定的ハッシュ**で対象 run を選ぶ。選ばれたら本線プロファイルではなく
  推奨チェーン(`recommended_chain`)の**次候補**を pin し、`routing_arm = "explore"`。
- 決定的: `std::hash::DefaultHasher` は使わず、issue 番号に対する小さな明示的ハッシュ
  (FNV-1a 等)で `hash % 10_000 < explore_ratio * 10_000` を判定。再現可能・テスト可能。
- 次候補が本線と同じ/`default` にしかならない役割では explore は no-op(本線のまま)。

## 触るファイル

- `src/config.rs` — `RoutingConfig` に `explore_ratio: f64`(既定 0.0)と
  `escalation: EscalationConfig` を追加。`EscalationConfig { enabled: bool(既定 true),
  #[serde(flatten)] roles: HashMap<String, Vec<String>> }`。`[routing.escalation]` の
  `enabled` と役割別チェーン(`worker = [...]`)を同じテーブルで受ける。
- `src/routing.rs` — `escalation_chain(role) -> Vec<String>`(既定表 + config 上書きの
  マージ)、`explore_alternative(cfg, role) -> Option<String>`、explore 対象判定
  `is_explore(issue_number, ratio) -> bool`(決定的ハッシュ)を追加。`recommended_chain`
  はそのまま(検出フォールバック用途、別物)。
- `src/engine/flow.rs` —
  - `Checkpoint` に `escalation_level: u32`(serde 既定 0)を追加。
  - `validate()`: 2 回目の fix ターン前に `maybe_escalate()` を呼ぶ。昇格処理
    (pin 上書き・ペイン retire・session_id クリア・`run.escalated` emit・
    `routing_arm='escalated'` 記録)を実装。
  - `resolve_run_profile()`: 未 pin の初回解決で explore 判定を挟み、explore なら
    代替プロファイルを pin して `routing_arm='explore'` + `run.explore_assigned` emit。
  - `ensure_pane()`: session_id が None のときは resume を試みず新規 spawn(既存挙動で
    足りるはず——クリアで表現できることを確認)。
- `src/store/runs.rs` + `src/store/migrations/0012_routing_arm.sql` —
  `ALTER TABLE runs ADD COLUMN routing_arm TEXT;`、`RunRecord` に `routing_arm`、
  `update_run_routing_arm(id, arm)` を追加。`src/store/mod.rs` の MIGRATIONS に登録。
- `src/store/stats.rs` + `src/app.rs` — `routing_stats` に routing_arm を集計軸として
  足し、`meguri stats routing` の表で本線 / explore / escalated を分けて表示。
- `src/events.rs` 経由の新イベント種別: `run.escalated`、`run.explore_assigned`
  (既存 `run.profile_resolved` と同じ書式)。

## 主要な決定(詳細は ADR 0012)

1. **エスカレーションチェーンは `recommended_chain` と別表**。前者は強くなる向き、後者は
   `default` に落ちる検出フォールバック。既定 `worker`/`fixer` = sonnet→opus、planner/
   reviewer 系はチェーンなし。
2. **昇格は resume せず新規セッション**。モデルが変わるため。文脈は失敗経緯をプロンプトで
   引き継ぐ。
3. **`runs.agent_profile` は現行プロファイル、履歴は events**。stats の本線/explore/
   escalated 区別のために `routing_arm` 列を新設。
4. **explore は決定的・既定 off**。実 issue を実験台にするため。

## 未決 / レビューで詰める点

- **needs_plan 昇格時のトリガー**(issue 記載の第 2 トリガー)。現状 needs_plan は worker
  run を終えて planner へ委譲する(別ループ・別 issue)ので、「同一 run 内で 1 段上げる」
  検証失敗トリガーとは性質が違う。**推奨**: needs_plan で issue が `ready` に戻された後の
  **次の worker run を 1 段上から始める**(#135 の振動ガードと同じ「一度詰まった issue は
  難しい」観測に乗せる)。ただし受け入れ条件は検証失敗トリガーのみを必須テスト対象と
  しているので、needs_plan 昇格は小さめの追随実装として分けてよい。実装可否と粒度を
  レビューで確定する。
- **explore の代替プロファイルの出所**。issue は「推奨表(recommended_chain)の次候補」と
  明記。escalation_chain の次(より強い)を使う案もあるが、issue の文言に従い
  recommended_chain を採る。
- **昇格が fix 予算を増やすか**。増やさない(=`validate_turns` を共有)を推奨:
  needs-human バックストップを素直に保つ。

## Migration & rollback(必須 / 永続状態に触れる)

- **Migration**: `0012_routing_arm.sql` は `ADD COLUMN routing_arm TEXT`(NULL 許容)。
  既存 run は NULL = 本線として読む(`agent_profile` と同じ後方互換の足し方)。データ変換・
  backfill は不要。`checkpoint_json` への `escalation_level` 追加は serde 既定 0 で
  吸収され、旧 checkpoint も読める。
- **Rollback**: 列は追加のみで破壊的変更なし。設定を `explore_ratio = 0` かつ
  `[routing.escalation]` 未設定に戻せば挙動は routing 1/3 と一致(コード上も新経路は
  inert)。列が残っても古いバイナリは無視する(SELECT が名指しする列だけ読む設計)ので、
  バイナリの巻き戻しも安全。破棄する場合の列削除は SQLite では table 再作成が要るため、
  実運用では「列は残し機能を config で無効化」がロールバック手順。

## Observability(必須)

- events: `run.escalated { from, to, level, reason }`、`run.explore_assigned
  { profile, alt_of }`、既存 `run.profile_resolved` は据え置き。
- `meguri stats routing`: `(role, profile)` に加え **arm(main/explore/escalated)** で
  分けて success rate・平均ターン数を表示。これで昇格率(escalated 行の割合)と explore の
  本線比較が読める(#65 の目的)。

## Test strategy(必須)

- **単体(routing.rs)**: `escalation_chain` の既定と config 上書きマージ、`is_explore`
  の決定性(同じ issue 番号は常に同じ判定、ratio=0 で常に false、ratio=1 で常に true)、
  `explore_alternative` の次候補選択と no-op ケース。
- **単体(config.rs)**: `[routing.escalation]` の `enabled` + 役割別チェーンのパース、
  `explore_ratio` 既定 0.0。
- **e2e(FakeMux、受け入れ条件そのまま)**:
  - 検証を 1 回目失敗→昇格後の 2 回目成功で返す fake agent → 2 プロファイルで spawn された
    こと(FakeMux の spawn 記録の command が sonnet→opus)、`run.escalated` が出たこと、
    2 回目 spawn が resume ではない(session クリア済み)ことを検証。
  - チェーン末尾でも失敗し続ける fake agent → needs-human に落ちる(無限昇格しない)。
  - `explore_ratio = 0` で routing 1/3 と同一の spawn になる回帰。
  - explore 対象になる issue 番号 + `explore_ratio` で代替プロファイル spawn + 
    `routing_arm='explore'` が stats に本線と別行で出る。

## 受け入れ条件(元 issue)

- [ ] 検証失敗 2 回目で昇格チェーンの次プロファイルに切り替わる(FakeMux e2e)
- [ ] 昇格時に resume ではなく新規セッション + 経緯入りトリガーで再開する
- [ ] チェーン末尾で更に失敗したら従来どおり needs-human へ(無限昇格しない)
- [ ] `explore_ratio = 0`(既定)で挙動が routing 1/3 と完全一致
- [ ] explore 割り当てが決定的で、stats 上で本線と区別できる
