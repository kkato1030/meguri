# issue-133 spec — planner の適応的 spec 深度(不確実性 × 影響範囲で必須セクションを変える)

issue は「全 issue に同じ深さの spec を書く」現状を、issue の性質に応じて深さを変える形に直せと言っている。調査の結論はシンプルだ: **これは planner の execute プロンプトの拡張とテストに閉じる**。新しいサブシステム・phase・ラベル・承認機構は作らない。フロー(`meguri:plan` → `meguri:speccing` → spec review → 実装)も、オーケストレータの状態機械も無変更。

深さを適応させるという設計判断そのもの(2 段・不確実性 × 影響範囲・veto 軸・新サブシステムを作らない・前方互換ヒント)は spec より長生きするので **ADR 0009(本 PR 同梱)** に置いた。この spec はその決定を実装へ落とすための足場であり、実装時に削除される。

## 決定(要約、詳細は ADR 0009)

- spec の深さは 2 段: **normal**(現状どおり)と **design**(深い)。多段化しない。
- 深さは**工数ではなく「不確実性 × 影響範囲」**で決める。planner の in-context 判断(spec を書くための repo 調査)に委ねる。
- **veto 軸**: 永続状態・schema・公開 contract への影響、または不可逆な運用リスクを検出したら、総合判断にかかわらず migration / rollback を必須にする(ハードなフロア)。
- design spec の必須セクション: architecture impact / 代替案と決定 / migration・rollback(state 影響時)/ observability / test strategy。
- `spec_depth: normal | design` ヒント(人間が issue 本文に明示、将来は triage #87 のコメント / marker)を尊重。ヒントは深さを上げられるが veto フロアより下げられない。
- 深さの判断理由を spec 本文(または PR 説明)に 1〜2 文残す。
- design spec も disposable(ADR 0001 不変)。実装時に ADR / ドメイン文書へ振り分けて破棄。

## 実装内容(この branch の続きでやること)

### 1. planner の execute プロンプトに「適応的 spec 深度」節を足す — `src/engine/planner.rs`

`PlannerFlavor::execute_prompt`(現状 `src/engine/planner.rs:116-163`)の Instructions と `decompose_section` の間に、新しい節を挿入する。既存の decompose 節が `decompose_instruction(issue_body)` というヘルパ関数で組み立てられているのと同じ形で、`adaptive_depth_instruction(issue_body)` を切り出すのがきれい(テストしやすく、`spec_depth:` ヒントの有無で文面を変えられる)。

節に必ず含める要素:

- **2 段の定義**: normal(現状の軽い spec)と design(深い spec)。
- **判定原則**: 「実装工数ではなく、不確実性 × 影響範囲で選べ。何が未確定か・間違えたときの被害範囲を列挙してから深さを決めよ」。
- **veto ルール**: 「永続状態 / schema / 公開 contract に触れる、または不可逆な運用リスクがあるなら、総合判断にかかわらず migration / rollback セクションを必須にせよ」。
- **design spec の必須セクション列挙**: architecture impact / 代替案と決定 / migration・rollback(state 影響時)/ observability / test strategy。
- **理由の明記**: 「選んだ深さの理由を spec 本文(または PR 説明)に 1〜2 文残せ」。
- **ヒントの尊重**: 「issue 本文(将来は triage のコメント / hidden marker)に `spec_depth: design` または `spec_depth: normal` があれば尊重せよ。`design` は深さのフロアを上げる。`normal` はヒントだが、veto が deep を要求する場合は veto が勝つ」。
- **disposable の再確認**: 「design spec も使い捨て。architecture / 決定は ADR、ドメイン規則はドメイン文書へ振り分け、残りは実装に蒸留して spec は消える。永続の設計文書ではない」。既存の disposable 指示(`planner.rs:135-144`)と矛盾しないよう、深い方も例外でないことを一言添える。

ヒントの取り回しは **プロンプトレベルに閉じる**(issue 本文はすでにプロンプトへ丸ごと差し込まれるので、エージェントは `spec_depth:` 行を読める)。`spec_depth` をコードでパースする必要はない — ADR 0009 の「in-context 判断・新サブシステムを作らない」に沿う。triage v1(#87)が着地したら triage コメント / marker をプロンプトへ渡す小さな追補で自動供給に接続する(本 issue のスコープ外)。

### 2. テスト — `src/engine/planner.rs`(ユニット)

`execute_prompt` / `adaptive_depth_instruction` に対して、既存の `prompt_*` テスト群(`planner.rs:414-483`)と同じ形で:

- プロンプトが「不確実性 × 影響範囲」「veto」「migration」「rollback」「design」といった要点を含む。
- design spec の必須セクション名(architecture impact / observability / test strategy 等)が現れる。
- 深さの理由を残す指示が現れる。
- `spec_depth: design` を含む issue 本文で、ヒントを尊重する旨がプロンプトに現れる(ヘルパを切り出す場合はその関数単体でも検証)。
- 回帰: normal 側の軽さを壊していない(既存の `prompt_demands_spec_not_implementation` / `prompt_routes_durable_value_out_of_the_disposable_spec` が通り続ける)。

E2E(`tests/planner_test.rs`)は happy-path のプロンプト検証(`planner_test.rs:311-318`)に「深度節が含まれること」の 1 アサーションを足す程度でよい。深さの実際の判定はエージェントの判断なので E2E では検証しない。

### 3. README の spec-first 節を 1〜2 文更新 — `README.md` / `README.ja.md`

spec-first flow の段落(`README.md:200` 近辺)に、「spec の深さは issue の性質(不確実性 × 影響範囲)で normal / design の 2 段に適応し、state / contract に触れる変更では migration / rollback を含む深い構成になる。判断理由は spec / PR に残る」旨を 1〜2 文で追記。フロー・ラベル表は無変更(この issue はラベルも phase も足さない)。

## 触るファイル

- `src/engine/planner.rs` — `execute_prompt` に適応的深度節を挿入(`adaptive_depth_instruction` ヘルパ + ユニットテスト)
- `tests/planner_test.rs` — happy-path プロンプト検証に深度節のアサーションを 1 つ追加
- `README.md` / `README.ja.md` — spec-first 節に適応的深度を 1〜2 文追記
- `docs/adr/0009-adaptive-spec-depth.md` — 決定の記録(本 PR に同梱済み)

## 受け入れ基準(元 issue のたたき台を実装形に落とす)

1. 永続状態や公開 contract に触れる issue に `meguri:plan` を貼ると、planner の execute プロンプトが veto ルールにより migration / rollback を含む深いセクション構成を要求する(プロンプトに veto と必須セクションが現れることをユニットテストで担保)。
2. 局所的で明確な issue の spec は現状どおり軽いまま(既存の normal 系プロンプトテストが回帰なしで通る)。
3. 深さの判断理由を spec 本文(または PR 説明)に 1〜2 文残す指示がプロンプトに含まれる。
4. spec review・実装フローは無変更で通る(ラベル遷移・状態機械・既存 E2E テストが非破壊)。
5. `spec_depth: design | normal` ヒントを尊重する指示がプロンプトに含まれ、ヒントは veto フロアより下げられない旨が明記される。

## スコープ外(やらないこと)

- 6 軸数値スコアの出力契約化、YAML ルールエンジン、`ElaborationPlan` 型、`meguri:elaborate` phase(ADR 0009 で棄却)。
- 深さの多段化(2 段で始める)。
- triage v1(#87)からの `spec_depth` 自動供給の配線(triage が存在してからの follow-up)。
- `spec_depth` のコード側パース(プロンプトレベルに閉じる)。
