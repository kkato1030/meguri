# issue-66 spec — routing (3/3): 詰まったら昇格 + explore_ratio

routing 1/3(#64)の静的な役割→プロファイル振り分けに、**難易度を推定せず**難易度へ
適応する 2 つの仕組みを足す。設計判断の理由は ADR 0012 に置いた——この spec は実装を
収束させるための足場で、着地時に消える。

## spec 深度の理由(design spec)

`runs` にスキーマ列を 1 つ足し(`routing_arm`)、公開設定契約を拡張し(`[routing]` に
`explore_ratio`、新トップレベル `[escalation]`)、`runs.agent_profile` の書き込み意味論
(昇格で上書きされうる)を変える。加えて session 再注入・チェーンの出所・stats 帰属・
設定配置が routing の on/off 契約と衝突しない置き方に本物の不確実性がある。よって
**design spec**、かつ永続状態に触れるので migration & rollback を必須セクションとして書く
(veto ルール)。

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

### C. 共通ゲート: routing が有効なときだけ働く

エスカレーションも explore も、**`[routing]` が存在して routing が有効なときだけ**動く。
legacy(`[routing]` なし)では両方 inert で、「全 role が `default`、検出なし」という
routing 1/3 以前の挙動をバイト単位で保つ。チェーンは推奨表に由来する概念なので、routing
が off の場所で昇格が走るのは筋が通らない。この共通ゲートが後述の設定配置と rollback の
土台になる。

## 設定の形(重要 — `[routing]` の存在判定を壊さない)

```toml
[routing]
explore_ratio = 0.0   # opt-in canary。既定 0.0 = off。explore は routing の割り当て方の
                      # 一部なので [routing] 内でよい(on にする時点で routing が要る)。

[escalation]          # トップレベル(ADR 0007 の [drift] と同型)。
enabled = true        # 既定 true。ただし routing が有効なときだけ効く(共通ゲート)。
# worker = ["claude-sonnet", "claude-opus"]   # 役割別チェーンの上書き
```

`escalation` を **`[routing.escalation]` にしない**のが要点。`[routing]` は「存在したら
role routing 有効」という契約(routing.rs の `Some(routing) else legacy`)なので、
`[routing.escalation] enabled = false` と書くだけで親 `[routing]` が生まれ、`mode = auto`
の推奨解決が勝手に有効化されてしまう。「昇格だけ止めたい」設定が legacy を壊す。ADR 0007 が
`[routing.drift]` を避けてトップレベル `[drift]` にしたのと同じ理由で、独立に止めたい
`escalation` はトップレベルに出す。一方 `explore_ratio` は既定 off で「on にするとき」しか
書かれず、その時点で routing を使う意思があるので `[routing]` 内でも罠にならない。

## 触るファイル

- `src/config.rs` —
  - `RoutingConfig` に `explore_ratio: f64`(既定 0.0)を追加。
  - **トップレベル**に `escalation: EscalationConfig`(`[escalation]` セクション、`[drift]`
    と同型)を追加。`EscalationConfig { enabled: bool(既定 true),
    #[serde(flatten)] roles: HashMap<String, Vec<String>> }` で `enabled` と役割別チェーン
    (`worker = [...]`)を同じテーブルから受ける。`[routing.escalation]` にはしない
    (上記「設定の形」の理由)。
  - エスカレーション・explore の判定は両方 `cfg.routing.is_some()` でゲートする
    (共通ゲート)。
- `src/routing.rs` — `escalation_chain(role) -> Vec<String>`(既定表 + config 上書きの
  マージ)、`explore_alternative(cfg, role) -> Option<String>`、explore 対象判定
  `is_explore(issue_number, ratio) -> bool`(決定的ハッシュ)を追加。`recommended_chain`
  はそのまま(検出フォールバック用途、別物)。
- `src/engine/flow.rs` —
  - `Checkpoint` に `escalation_level: u32`(serde 既定 0)を追加。
  - `validate()`: 2 回目の fix ターン前に `maybe_escalate()` を呼ぶ。昇格処理
    (pin 上書き・ペイン retire・session_id クリア・`run.escalated` emit)を実装。
    腕の記録は explore を優先する(下記の腕優先ルール):本線 run なら
    `routing_arm='escalated'` を立てるが、すでに `explore` の run は `explore` の
    まま据え置く(昇格自体は `run.escalated` イベントに残る)。
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
5. **腕優先ルール(explore > escalated > main)**。1 つの run は `routing_arm` を 1 語しか
   持たない。explore で始まった run が後で昇格しても腕は `explore` のまま(ADR 0012 の
   「explore の腕記録を優先」)。理由:explore は「代替プロファイルと本線を比べる」ための
   母集団で、そこから昇格した run を `escalated` に移すと比較の分母が崩れる。昇格の事実は
   `run.escalated` イベントに残るので観測は失われない。
6. **`escalation` はトップレベル、両機能は routing 有効時だけ働く**。`[routing]` の存在が
   role routing の on/off 契約なので、独立に止めたい `escalation` を `[routing.escalation]`
   にすると「昇格だけ止める」設定が legacy を壊す。ADR 0007 の `[drift]` と同じくトップ
   レベル `[escalation]` にし、さらに両機能を `cfg.routing.is_some()` でゲートして、
   legacy を無傷に保つ。

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
- **Rollback**: 列は追加のみで破壊的変更なし。2 つの仕組みは独立に config で止められ、
  どちらの停止も `[routing]` の存在判定に触れない(=role routing の on/off を変えない):
  - **legacy(`[routing]` なし)**: 共通ゲートによりエスカレーションも explore も最初から
    inert。何も書かなくても routing 1/3 以前とバイト単位で一致。
  - **explore を止める**: `explore_ratio = 0`(既定)。explore 経路は inert、割り当て軸は
    routing 1/3 と一致。
  - **エスカレーションを止める**: トップレベル `[escalation]` に `enabled = false`。これは
    `[routing]` を生成しないので、role routing の有効/無効は現状のまま変わらない。
    エスカレーションは既定 `enabled = true` で `worker`/`fixer` に既定チェーンがあるため、
    routing 有効環境で検証失敗時の昇格まで含めて routing 1/3 と完全一致させたいときは、この
    明示 off が必要(未設定のままだと昇格は動く)。
  - routing 有効環境で両方止めれば(`explore_ratio = 0` + `[escalation] enabled = false`)、
    挙動は routing 1/3 とバイト単位で一致する。
  列が残っても古いバイナリは無視する(SELECT が名指しする列だけ読む設計)ので、バイナリの
  巻き戻しも安全。破棄する場合の列削除は SQLite では table 再作成が要るため、実運用では
  「列は残し機能を config で無効化」がロールバック手順。

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
- **単体(config.rs)**: トップレベル `[escalation]` の `enabled` + 役割別チェーンのパース、
  `explore_ratio` 既定 0.0、そして **`[escalation]` を書いても `cfg.routing` は None のまま**
  (role routing を勝手に有効化しない)ことの回帰。
- **e2e(FakeMux、受け入れ条件そのまま)**:
  - 検証を 1 回目失敗→昇格後の 2 回目成功で返す fake agent → 2 プロファイルで spawn された
    こと(FakeMux の spawn 記録の command が sonnet→opus)、`run.escalated` が出たこと、
    2 回目 spawn が resume ではない(session クリア済み)ことを検証。
  - チェーン末尾でも失敗し続ける fake agent → needs-human に落ちる(無限昇格しない)。
  - explore 経路の回帰: `explore_ratio = 0` で explore 割り当てが起きず、profile 解決が
    routing 1/3 と同一になる(explore 軸のみの回帰。昇格軸は下の rollback テストで担保)。
  - legacy 回帰: `[routing]` なし + `[escalation]`/`explore_ratio` 記載あり → 昇格も
    explore も起きず、全 role が `default` の routing 1/3 以前と同一 spawn。
  - explore 対象になる issue 番号 + `explore_ratio` で代替プロファイル spawn + 
    `routing_arm='explore'` が stats に本線と別行で出る。
  - **腕優先**: explore で始まった run が昇格しても `routing_arm` は `explore` のまま
    (`escalated` に上書きされない)こと、かつ `run.escalated` は emit されること。
  - **昇格の rollback**: routing 有効環境で `[escalation] enabled = false` にすると検証失敗
    しても昇格せず、routing 1/3 と同一挙動になること。

## 受け入れ条件(元 issue)

- [ ] 検証失敗 2 回目で昇格チェーンの次プロファイルに切り替わる(FakeMux e2e)
- [ ] 昇格時に resume ではなく新規セッション + 経緯入りトリガーで再開する
- [ ] チェーン末尾で更に失敗したら従来どおり needs-human へ(無限昇格しない)
- [ ] `explore_ratio = 0`(既定)で **explore 経路が inert** になり、割り当ては routing
      1/3 と一致(explore 軸の一致。昇格軸まで含めた完全一致は `[escalation] enabled = false`
      も併せる — 本文 rollback 参照。legacy では両軸とも自動で inert)
- [ ] explore 割り当てが決定的で、stats 上で本線と区別できる
