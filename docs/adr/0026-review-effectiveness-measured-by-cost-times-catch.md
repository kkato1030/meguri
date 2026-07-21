# ADR 0026: レビューの効き目は COST(token) と CATCH(捕捉)の二軸で測る — 比較指標は効率(捕捉/token)、実行時 union は不変

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

**レビューの効き目を COST(token) と CATCH(捕捉)の独立した二軸で計測する。両者の積を「効き目
スコア」として採用することはしない**(f1 — 積だと token が増えるほど値が大きくなり、同じ捕捉
でも高コストな編成を良く見せてしまい、二つの軸の目的関数が矛盾する)**。二軸を比較可能な形に
落とす派生指標は 効率 = 捕捉 / token の一つに絞り、「同じ捕捉なら token が少ないほど良い、
同じ token なら捕捉が多いほど良い」の一方向だけを改善と判定する。実行時の finding merge
(無差別 union、ADR 0020/0023)は本 ADR でも不変のままにする。**

### 軸A: COST

ターン完了時に、そのターンが載せたコーディングエージェント CLI のトランスクリプトから
usage(往復数・ピーク文脈・処理 input/output token)を集計する **telemetry sidecar** を置く。

- **read するが、成否裁定には使わない。** completion contract の3条件
  (git tree clean・base より進んでいる・`check_command`)には一切食い込ませない —
  計測が実行時の不変条件を変えないという ADR 0020 の立て付けをそのまま踏襲する。
- **backend 非依存。** コストは meguri ではなく載せている CLI 側の自律ループが背負っている
  以上、sidecar は特定の CLI 実装に縛られない形で usage を読めなければならない。
- **記録粒度は「ターン」単位、join key は `(run_id, turn_id, lane)`**(f6)。round1 並列
  reviewer(ADR 0023)は設定エントリごとに `self-review#<index>` という専用 lane の独立ターン
  としてすでに動いている(各 reviewer が自分専用のペイン・トランスクリプトを持つ)。したがって
  reviewer 別の token を按分・N 等分する必要は無い — そのターンの usage がそのまま、その
  reviewer に帰属する。
- **効率式の分母 `token` は「そのターンの transcript usage から集計した input 系
  (cache read / cache creation を含む)と output の総和(raw 値・課金補正なし)」に固定する**。
  単位は 1k token(指標は `exclusive_catch / 1k token`)。sidecar は内訳
  (`input_tok / cache_read_tok / output_tok / round_trips` 等)を個別に記録するが、
  比較指標の分母はこの総和一本であり、slice ごとに別の分母(input のみ・課金 token 等)を
  実装してはならない — backend 非依存を掲げる以上、分母が揺れると reviewer 間・backend 間の
  比較が再現できない。内訳の一部しか復元できず総和を構成できないターンは、下記の
  「実行後 usage 欠損」として扱う(部分値で効率を出さない)。
- **usage 欠損は「事前 drop」と「実行後欠損」を区別して記録し、静かに比較母集団から
  消さない**(f7)。現行実装の `EVENT_REVIEWER_DROPPED`(`src/engine/self_review.rs`)は
  (a) profile 未検出で fan-out **前**に弾かれた場合と、(b) fan-out **後**にターンが
  pane death・エラー・不正な出力で落ちた場合の両方で発行される — この2つを同一視しては
  ならない。(a) はプロセスを起動していないので cost も catch も真にゼロであり、比較母集団
  (対象ターン総数)からも外してよい。(b) は token を消費している可能性があるので対象ターン
  総数に算入し、usage を復元できなければ「usage 不明」として欠損率に数える(トランスクリプト
  形式を読めなかった完走ターンも同じ「usage 不明」)。段階1(sidecar)は、この2状態を
  区別できるようイベントを分割するか `stage: pre_fanout | post_start` のような判別子を
  足すところまでを含む。(b) を静かに分母から除くと、途中失敗しやすい高コスト reviewer ほど
  効率を過大評価してしまう。効率(`exclusive_catch/token`)は usage が既知の行だけで算出して
  よいが、その数字には必ず coverage(usage 既知のターン数 / 対象ターン総数)と欠損率を併記し、
  比較母集団が縮んでいることを常に見える形にする。

### 軸B: CATCH

台帳(ADR 0022/0023 の findings ledger)から reviewer 単位の捕捉を導出する。

- **reviewer の識別単位は `reviewer_profile` ではなく設定エントリの `reviewer_index`(round1
  並列 reviewer の `[[review.reviewers]]` 上の位置、0始まり)とする**(f5)。ADR 0023 §3 の
  「単一モデル構成 → lens 分割」では、同じ profile の複数エントリが異なる lens を持つため、
  profile 名だけを識別子にすると lens 分割された reviewer の寄与が1つに潰れて排他捕捉の
  数え上げを誤る。`Finding`/`LedgerEntry` に(既存の `reviewer_profile` は残したまま)
  `reviewer_index` を追加する — round1 の fan-out はすでに `ReviewerPlan.index` /
  `self-review#<index>` lane としてこの index を内部で持っているので、それを ledger まで
  引き通すだけでよい。checkpoint 側の追加フィールドであり、DB スキーマ変更は伴わない。
- **指標名は `unique_fixed` ではなく `exclusive_catch`(reviewer 排他捕捉)とする**(f2)。
  round 1 の機械的 union(ADR 0023)は reviewer ごとに別 id を振るので、同じ指摘を複数
  reviewer が出しても id だけでは同一と分からない。集計クエリ側で同一フェーズ内の
  entry を `path` 一致 かつ `line` の差が3行以内なら同一指摘とみなす同値化ルールで束ね、
  その同値クラスに `reviewer_index` が1種類しか無い entry だけを `exclusive_catch` と数える。
  既存の `path`/`line`/`reviewer_index` から導出でき、新しいイベント・スキーマは要らない。
  **これは観測可能な代理指標であり、「その reviewer を抜いたら本当に拾えなかったか」という
  反事実の証明ではない。** 真の限界寄与は下記 §反事実 の leave-one-out canary(opt-in)でのみ
  検証する。
- **numerator は `fixed` のみとする**(f3)。Phase1 の ground truth は台帳の `fixed`
  (reviewer が解消を確認した状態)だけを捕捉に数え、`waived`(作者が反対し reviewer がその
  扱いを確認しただけの状態)は含めない — waived は「直った証拠」ではないからだ。waived は
  別カウンタ `waive_rate` として並記し、捕捉とは混ぜない。Phase2 で revert / CI / reopen と
  いった下流シグナル(§スコープ 3)が入れば、fixed のうち実際に効いたものだけへ ground truth
  をさらに絞り込む。
- **`blocking_saves`(guard が防いだ損害数)は Phase1 では定義しない**(f3)。guard の
  `blocking` 件数だけでは false positive や人間が却下した停止まで「救った」件数に混ざる。
  実害の有無を確認できる Phase2 まで `blocking_saves` の算出を先送りする。
- **Phase1 で出す指標はこの2つに限る**: reviewer 別 `exclusive_catch / 1k token`、cap 到達率
  × コストの交差(cap に落ちる編成ほど高コストか、という相関であって救済数の主張ではない)。

### 反事実(canary)は段階4まで先送りする

観察データ(observational)を先行させる。**編成変更(reviewer の採否・並列数の増減)の意思決定
時にのみ opt-in で回す専用の leave-one-out canary を段階4で新設する**(f4)。ADR 0013 の
`explore_ratio` は auto routing が issue 単位で推奨 profile と次候補を振り分ける仕組みであり、
reviewer の採否や並列数 N を treatment として割り当てる仕組みではないため、そのまま
「再利用」はできない。段階4は既存 canary の流用ではなく、reviewer 構成(フル構成 / 1本抜き)
そのものを issue 単位の arm として割り当て、`exclusive_catch` の反事実を測る専用の設定
スナップショットと比較単位を新設する。常時の A/B ではない — ADR 0017/0020 が守ってきた
「観察データは相関であって因果の証明ではない」という正直な位置づけを本 ADR でも崩さない。

## スコープ(段階導入)

各段は本 issue とは別の slice として切り出す。本 issue(#236)は ADR 0026 の決定と
段階分割そのものを確定させる**追跡 issue**であり、以下のどの段の実装コードも含まない。

1. **sidecar**(COST 記録) — telemetry sidecar を実装し、ターン完了時に usage を記録する。
   `EVENT_REVIEWER_DROPPED` の事前 drop / 実行後 drop の区別(イベント分割または判別子の
   追加)もこの段に含む。
2. **`meguri stats review` 拡張**(COST と CATCH の join ビュー) — 1 の記録と台帳から導出した
   `exclusive_catch` を join し、reviewer 別の効率(`exclusive_catch / token`)と、その
   coverage・欠損率(f7)をあわせて出す。
3. **下流シグナル**(Phase2: revert / CI / reopen) — CATCH の ground truth を広げ、
   `blocking_saves` を定義可能にする。
4. **canary**(opt-in) — reviewer 構成(フル構成 / 1本抜き)を issue 単位の arm として割り当てる
   専用の leave-one-out canary を新設し、編成変更の意思決定時だけ回す。

## Consequences

- **「品質担保に必要か、過剰か」を編成の変更なしに問えるようになる。** sidecar と
  join ビューが揃えば、reviewer 追加・guard 縮退のたびに「recall は上がったか」ではなく
  「token あたりの捕捉(効率)は上がったか」で判断できる。ただし Phase1 の `exclusive_catch`
  は観測できる代理指標であり、真の反事実(§反事実)を証明するのは段階4の leave-one-out
  canary だけである点は判断のたびに意識する。
- **completion contract・実行時 merge は無傷。** COST 計測は read-only の telemetry で、
  git tree・`check_command` の3条件にも、self-review/guard の無差別 union merge
  (ADR 0020/0023)にも触れない。
- **段階間に依存がある。** 2(join ビュー)は 1(sidecar)の記録が無ければ書けない。
  4(canary)は 1〜3 が生む指標が無ければ「何を比較したいか」自体を決められない。
  この依存順のまま、各段を個別 issue として切り出す。
- **backend 非依存という制約が sidecar の実装難度を上げる。** コストは meguri 本体ではなく
  ペインに載せた CLI の自律ループ側にあるため、sidecar は特定 CLI のトランスクリプト形式に
  結合しすぎない読み取り層を持つ必要がある — 詳細設計は段階1の spec に委ねる。
- **ledger の checkpoint フィールドが1つ増える。** `Finding`/`LedgerEntry` に `reviewer_index`
  を足す(段階2)。DB スキーマ変更ではなく、`reviewer_profile` 追加(ADR 0023)と同じ性質の
  追加フィールドで、単一 reviewer 経路の checkpoint は byte-for-byte のまま変わらない。
- **効率の数字は必ず coverage 付きで出す。** usage を復元できなかったターンを「無かったこと」
  にして分母を静かに縮める設計は、途中失敗しやすい高コスト reviewer ほど効率を過大評価する
  危険がある(f7)。段階2の join ビューは効率と一緒に coverage・欠損率を常に表示し、比較
  母集団が縮んでいることを隠さない。
- **観察データのまま留める判断を明示する。** canary(4)を「常時」ではなく「編成変更の
  意思決定時だけ」の opt-in にしたのは、観察データの相関を安易に因果へ格上げしないための
  歯止めであり、ADR 0017/0020 と同型の位置づけである。
