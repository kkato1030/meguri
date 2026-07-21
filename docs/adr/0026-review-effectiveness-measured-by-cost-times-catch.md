# ADR 0026: レビューの効き目は COST(token) × CATCH(限界catch) で測る — 効率 = 限界catch/token、実行時 union は不変

- Status: proposed
- Date: 2026-07-21
- Issue: #236(親: #211、前提: #213(ADR 0020) / #212(ADR 0022) / #214(ADR 0023) / #228(ADR 0025))
- 関連: ADR 0020(自己レビューの効き目は統率面イベントで測る・merge は無差別 union のまま)、
  ADR 0022(findings 台帳・挙動 escalation)、ADR 0023(round1 並列 reviewer)、
  ADR 0025(guard は安全 tripwire)、ADR 0013(profile escalation と explore canary)

## Context

「今のレビュー編成(自己レビューの並列 reviewer・guard)は品質担保に必要なのか、過剰なのか」を
データで問えない。

- コストは meguri 自身ではなく、meguri がペインに載せる claude code cli 側の自律ループ(review
  turn 内の `Agent()` サブレビュアー fan-out を含む)が背負っている。実測では pr-reviewer の
  1レビューが **45往復・ピーク文脈 132K token・処理 input ≒ 390万 token** に達した。
- ADR 0020(#213、`meguri stats review`)と ADR 0022/0023 の台帳は、cap 落ち率・round 分布・
  waive 率という **CATCH 側の一部**を測る。だが **COST(token) は一切測っていない**。片肺の
  計測では「効率」を語れず、編成が正しいかどうかの判断材料にならない。
- ADR 0023 の round1 並列 fan-out は「recall が上がるはず」という仮説で導入したが、その
  限界 recall(これ以上 reviewer を増やしても新しく拾えなくなる点)を測る手段が無い。
  ADR 0025 で guard は品質ゲートから安全 tripwire へ退いたが、退いた後もフルレビューの
  コストは変わっていない(over-provisioning の疑い)。両方とも仮説のまま検定できずにいる。

## Decision

**レビューの効き目を COST(token) × CATCH(限界catch) の積で測る。効率 = 限界catch / token。
実行時の finding merge(無差別 union、ADR 0020/0023)は本 ADR でも不変のままにする。**

### 軸A: COST

ターン完了時に、そのターンが載せたコーディングエージェント CLI のトランスクリプトから
usage(往復数・ピーク文脈・処理 input/output token)を集計する **telemetry sidecar** を置く。

- **read するが、成否裁定には使わない。** completion contract の3条件
  (git tree clean・base より進んでいる・`check_command`)には一切食い込ませない —
  計測が実行時の不変条件を変えないという ADR 0020 の立て付けをそのまま踏襲する。
- **backend 非依存。** コストは meguri ではなく載せている CLI 側の自律ループが背負っている
  以上、sidecar は特定の CLI 実装に縛られない形で usage を読めなければならない。

### 軸B: CATCH

台帳(ADR 0022/0023 の findings ledger)から `unique_fixed`(reviewer 単位で「その reviewer が
出さなければ誰も拾わなかった finding」)を導出する。

- ground truth は段階導入で、**Phase1** は台帳の `fixed` / `waived`(自己申告 + reviewer
  確認済みの状態)、**Phase2** は revert / CI / reopen といった下流シグナル(§スコープ 3)に
  広げる。
- **導出できる指標の例**: reviewer 別 `unique_fixed / 1k token`、guard の
  `blocking_saves / token`(ADR 0025 の blocking カテゴリ救済数を token で割る)、
  cap 到達率 × コストの交差(cap に落ちる編成ほど高コストか)。

### 反事実(canary)は先送りにする

観察データ(observational)を先行させる。ADR 0013 の `explore_ratio` canary は再利用するが、
**編成変更(reviewer の採否・並列数の増減)の意思決定時にのみ opt-in** で回す。常時の A/B では
ない — ADR 0017/0020 が守ってきた「観察データは相関であって因果の証明ではない」という
正直な位置づけを本 ADR でも崩さない。

## スコープ(段階導入)

各段は本 issue とは別の slice として切り出す。本 issue(#236)は ADR 0026 の決定と
段階分割そのものを確定させる**追跡 issue**であり、以下のどの段の実装コードも含まない。

1. **sidecar**(COST 記録) — telemetry sidecar を実装し、ターン完了時に usage を記録する。
2. **`meguri stats review` 拡張**(COST×CATCH の join ビュー) — 1 の記録と台帳の
   `unique_fixed` を join し、reviewer 別の効率を出す。
3. **下流シグナル**(Phase2: revert / CI / reopen) — CATCH の ground truth を広げる。
4. **canary**(opt-in) — `explore_ratio` を編成変更の意思決定時だけ回す経路を作る。

## Consequences

- **「品質担保に必要か、過剰か」を編成の変更なしに問えるようになる。** sidecar と
  join ビューが揃えば、reviewer 追加・guard 縮退のたびに「recall は上がったか」ではなく
  「token あたりの限界catch は上がったか」で判断できる。
- **completion contract・実行時 merge は無傷。** COST 計測は read-only の telemetry で、
  git tree・`check_command` の3条件にも、self-review/guard の無差別 union merge
  (ADR 0020/0023)にも触れない。
- **段階間に依存がある。** 2(join ビュー)は 1(sidecar)の記録が無ければ書けない。
  4(canary)は 1〜3 が生む指標が無ければ「何を比較したいか」自体を決められない。
  この依存順のまま、各段を個別 issue として切り出す。
- **backend 非依存という制約が sidecar の実装難度を上げる。** コストは meguri 本体ではなく
  ペインに載せた CLI の自律ループ側にあるため、sidecar は特定 CLI のトランスクリプト形式に
  結合しすぎない読み取り層を持つ必要がある — 詳細設計は段階1の spec に委ねる。
- **観察データのまま留める判断を明示する。** canary(4)を「常時」ではなく「編成変更の
  意思決定時だけ」の opt-in にしたのは、観察データの相関を安易に因果へ格上げしないための
  歯止めであり、ADR 0017/0020 と同型の位置づけである。
