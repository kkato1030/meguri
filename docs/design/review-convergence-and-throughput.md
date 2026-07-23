# 設計書: review 収束とスループット — ping-pong の出口設計と時間損失の解体

- Date: 2026-07-23
- Status: 提案(個別改善は issue 化して個別に判断する)
- 先行文書: `docs/design/needs-human-friction-and-delivery-speed.md`(2026-07-21、以下「先行設計」)、
  ADR 0022(台帳・挙動 escalation)/ 0023(round-1 並列)/ 0025(guard tripwire)/ 0026(COST×CATCH)
- 調査範囲: 2026-07-21(新バイナリ稼働)〜 07-23 の events / runs / turns テーブル全件、
  self-review 台帳の実データ(checkpoint_json)、現行コード(`src/engine/self_review.rs` /
  `src/engine/pr_reviewer.rs` / `src/turn/mod.rs`)

## 1. なぜ書くか

ADR 0022/0023/0025 の実装と daemon 向き直し(7/21)で「回数 cap による機械的な不収束」は
解消されたが、体感は変わっていない: **self-review は依然 3 round に到達しがちで、
guard には毎回のように止められ、issue 完了までの時間が長い**。本書は新バイナリ稼働後の
データだけで問題を測り直し、いま効く残りのレバーを設計する。

要旨: 収束問題は「cap 到達」から **「設計判断級 finding の ping-pong」と「guard blocking の
カテゴリ・インフレ」** に形を変えた。速度問題の実体は agent の作業時間ではなく、
**(a) escalate 後に人間を待ち続ける stuck turn(12〜14時間/件)、(b) 他プロジェクトの
クラッシュループによるスロット浪費、(c) 沈黙 skip ループ、(d) 不要な needs-human 1回 ≒
一晩の往復** である。収束の改善はそのまま速度の改善になる。

## 2. 実測(2026-07-21 〜 07-23)

### 2-1. 収束

- ADR 0022 の挙動 escalation は稼働している。現在の escalation はほぼすべて
  **ping-pong(同一 finding が 2 fix turns 経ても open)** 型:
  - meguri #245: finding f2 → 裁定後の再走で f3 → さらに f4 と **3連続で別 finding が
    ping-pong**。1件ごとに人間往復(ほぼ一晩)を消費し、62 run・成功 0・経過 41.7h。
  - meguri #235 f1(「実機 injection test を書け」)、#246 f4、air #1007 f3(×2回)。
- ping-pong する finding の中身は共通して**設計判断級**: 「削除セマンティクスを設計し
  受入条件を追加せよ」「実機テストで固定せよ」— issue スコープを超える作業要求か、
  『十分に直った』の基準が主観的なもの。作者は fixed を主張し、reviewer が open を維持する。
- 一方 **kind=decision + waive_reason(決定の記録)は機能している**(#1007 f2/f4 は決定が
  記録され再論なし)。問題は、設計判断級の要求が reviewer によって `defect` に分類される
  こと — 分類者 = 要求者なので、decision の出口に乗らない。
- round 分布はなお r3 偏重(meguri planner: r1×2 r2×7 r3×11、全期間 cap 率 61%)。
  07-23 は reviewed 9 / fixed 8 / **clean 0**。
- guard(impl): blocking 8 / advisory 11 / clean 4。**blocking 8 件は 3 PR(#231 #239 #254)に
  対するもので、カテゴリは全件 `cost`/`performance`**。security / data-loss の blocking は
  0 件。#254(インフライベント経路の修正)が cost+performance で3回止まった —
  閉じたカテゴリ制(ADR 0025)は、**検証されない緩いカテゴリ(cost/perf)を名指しするだけで
  素通し**になっている。guard(plan): blocking 12 / clean 3(5 PR 中ほぼ全 PR が最低1回
  blocking — spec_fixer が捌く設計どおりだが round 数はかさむ)。

### 2-2. 速度

issue 単位の経過時間(elapsed)と実 run 時間の乖離が大きい(#250: 65分の run で経過 15.9h、
#224: 経過 42.8h)。乖離の中身は次の4クラス:

1. **stuck turn が「人間待ち」で放置される**: `runtime_budget_exceeded` / `agent_quiet` は
   escalate するだけで turn を殺さない(`src/turn/mod.rs` の「escalate (don't kill)」)。
   #247 planner execute 738分、#251 conflict-resolver 709分、#245 planner 871分 —
   1件で半日〜一晩を失う。先行設計 P1(#245)の対象そのものだが、**その #245 自身が
   ping-pong で止まっている**(収束問題が速度改善をブロックする構図)。
2. **クラッシュループのスロット浪費**: jaburo-docs / jaburo-infra で 2日間に
   **pane_died 348 件**(1周 約0.1〜1.3分で即死→interrupted→再試行)。バックオフも
   circuit breaker もなく、`max_concurrent_runs = 10`(グローバル)を壊れたプロジェクトが
   食い続ける。原因は初回対話ゲート(#232/#235)ないし profile 不備とみられるが、
   **どの原因であれ「即死を無限に高速リトライする」構造が損失**。
3. **沈黙 skip ループ**: 「N open PRs resolve to issue — not picking one」
   (`src/engine/pr_reviewer.rs:467`)が #246 で **1317 回**、#245 で 60 回。PR は
   約40時間レビューされずに滞留し、イベントにも通知にも上がらない。
4. **review 税**: spec 系は execute 6〜13分に対し self-review+fix が 27〜54分(2〜4倍)。
   1 round ≒ review 8〜30分 + fix 10〜26分で、r3 偏重がそのまま時間に乗る。
   (worker の impl はバランスが健全: execute 15〜20分 / review+fix 約20分)
5. **escalation 再駆動のたびに重複 draft PR が増える**: #245 には open draft PR が
   **3本**(#257 / #261 / #262、いずれも issue-245 の spec を新規追加する 340〜390 行)。
   ping-pong escalation ごとに needs-human draft publish(#209 の fallback)が働くのは
   設計どおりだが、再駆動された planner が**既存 draft を adopt せず毎回新しい branch で
   ゼロから書き直す**ため、escalation 1回 = 重複 PR 1本になる。前 round までの
   収束(台帳・修正)も branch ごと捨てられる。3本の open PR は上記 3(pr-reviewer の
   「not picking one」skip ループ)の直接の燃料でもある。おまけに #257/#261 は
   どちらも「ADR 0028」を名乗り、main に既に存在する ADR 0028(infra-command-failures)と
   番号衝突している。#253(レール外 PR のブロック)は自レール branch を対象外とするため
   この重複は素通りする。

つまり速度の主犯は「agent が遅い」ではなく「**止まり方・待ち方・回り直し方**」にある。

## 3. 診断 — ping-pong の構造

現行の台帳(ADR 0022)は finding のステータスを動かせるのが reviewer だけで、
収束条件は「reviewer が omit するか」。これは次の3点で詰む:

- **閉じる基準が finding に書かれていない**: 「〜を設計してください」の『十分』は
  reviewer の主観で、fix ×2 と re-list ×2 が平行線をたどる。
- **作者に出口が waive しかない**: waive は「対応しない」宣言であり、「正しい指摘だが
  この issue のスコープ外/別 issue で対応すべき」を表現できない。実際の ping-pong 案件の
  多くは後者(スコープ拡張要求)。
- **分類者 = 要求者**: decision の出口(記録すれば閉じる・再論は needs_human)は機能して
  いるのに、設計判断級の要求が `defect` と分類されると乗れない。

guard 側は対称の問題: blocking の閉じたカテゴリ制は「カテゴリを名指ししたか」しか
検査せず(`pr_reviewer.rs` `read_review`)、**具体的な被害シナリオの提示を要求しない**。
security/data-loss は偽装しにくいが cost/performance は何にでも貼れる。

## 4. 設計

### Track A: 収束 — ping-pong に機械的な出口を作る

#### A1. finding に「閉じる条件」(closes_when)を必須化

finding スキーマ(`Finding`)に `closes_when` を追加: 「何がどうなればこの finding は
閉じるか」を**検査可能な1文**で reviewer に書かせる(例: 「spec に削除方式の節があり、
空レスポンス時の挙動が明記されている」)。round 2+ の reviewer への指示を
「open finding は **closes_when を満たしたかだけ**で判定せよ。基準を後から動かすな」に
変更する。基準の後出しを構造的に禁止し、fixed 裁定の主観の幅を絞る。

- 受け入れ: round 2+ の re-list には「closes_when のどこが未達か」の明示が必須。
  未記載の re-list は orchestrator が拒否(id validation と同じ層)。

#### A2. 作者側の第3の出口: `deferred`(follow-up issue 化)

fix turn の disposition に `deferred` を追加する。意味は「指摘は正しいが、この issue の
スコープを超える。follow-up issue で対応する」。

- deferred は台帳上ステータス `deferred` で閉じる(open 数に入らない)。converge 時に
  meguri が finding 本文から **follow-up issue を自動起票**し、PR body にリンクする。
  作業は失われず、PR は進む。
- reviewer が deferral に異議を出せるのは **guard と同じ閉じた安全カテゴリ
  (security / data-loss)を名指しした時だけ**。名指しがあれば即 needs_human(真の判断)、
  なければ deferral は成立。cost/perf は deferral を止められない — 「いま直すか後で直すか」は
  安全問題でない限り作者の裁量、という現実のレビュー規範に合わせる。
- ゲーミング対策は計測で受ける: `meguri stats review` に role/profile 別 deferred 率を出し、
  異常に高い作者 profile を人間がチューニングする(ADR 0026 の思想)。

#### A3. 2回目 open は「対立の decision 化」で終わらせる

ping-pong 検出(`fix_attempts >= 2 && open`)で即 needs_human にせず、**最終 fix turn で
作者に三択を強制する**: (a) もう一度直す(最後の1回) (b) deferred(→A2) (c) decision として
立場を記録(「この指摘には X の理由で対応しない」を waive_reason に記録)。
(c) の記録済み decision への再論は既存規則どおり needs_human — ただしその時の
escalation 文面は「A: 作者の立場 / B: reviewer の立場、どちらを採るか」の
**二択の decision 質問**として出す(現状の finding 全文貼り付けは人間の裁定コストが高い)。

- 効果: needs_human に届くのは「安全カテゴリ付きの deferral 異議」と「記録済み決定への
  再論」だけになり、#245 型の3連続一晩往復が消える。#245/#235/#1007 の実ケースを
  fixture 化して受け入れテストにする。

#### A4. round-1 並列 reviewer を有効化する(sequential discovery 対策)

f2→f3→f4 と**1件ずつ順番に** ping-pong するのは、後続 round で新規 finding が
出続けるから。round 1 で recall を出し切る設計(ADR 0023)は実装済み・config 待ちなので
有効化する。ただし gpt profile の stuck session 前歴(context 100% 恒久 400)があるため、
**#245(セッション健全性)の着地を先行条件**とするか、当面 anchor(claude)+grok の
2本構成で始める。効果測定は `meguri stats review` の round 分布と ping-pong 率で。

#### A5. guard blocking に「被害シナリオ」を要求し、素通しカテゴリを塞ぐ

blocking の必須条件を「カテゴリ名指し」から「カテゴリ + **具体的な被害シナリオ**
(何がどう壊れる/いくら失われる)」に強化する。シナリオ欠落の blocking は
orchestrator が advisory に降格して記録する(ADR 0025 の tripwire 思想の徹底 —
止める側に挙証責任)。あわせて計測: blocking が人間裁定で維持された率
(sustained rate)を stats に出し、恒常的に低いカテゴリは blocking 資格の見直し対象にする。
実装済みの #248(impl 側 bounded fixer)はこの上に載せる — 正当な blocking は自動一次対応、
不当な blocking はそもそも発生しない、の二段構え。

#### A6. anchor 照合と reviewer session(先行設計 P3 の継承)

finding に `quote`(現物引用)を足し、head に引用が存在しない finding は stale として
1回だけ差し戻す。reviewer turn は fresh session を既定にする(現行は self-review round 跨ぎ・
pr-reviewer ラウンド跨ぎで session 再利用 — 旧 head の記憶が現物に勝つ 3-B の構造温床)。
内容は先行設計 P3 のとおり。A1 の closes_when 判定が「現物参照」を前提にするため、
A1 と同時期に入れるのが望ましい。

### Track B: 速度 — 止まり方・待ち方・回り直し方

#### B1. #245 を先に通す(人間アクション、コード変更なし)

最大の時間損失(stuck turn 12〜14時間/件)の対策は #245 で設計済みなのに、
その #245 が ping-pong で止まっている。**残っている f4 finding を人間がいま裁定して
着地させる**のが、コード1行より効く最初の一手。Track A はこの種の膠着の再発防止。

#### B2. escalate 後の自動再試行(stuck turn を人間待ちで放置しない)

`runtime_budget_exceeded` / `agent_quiet` の escalate 後、pane/agent が実際に死んでいる
(AgentState で判定)なら、人間を待たずに **K 回まで自動 recover**(pane kill → session clear →
fresh spawn、prompt は自己完結なので文脈再構築不要)。K 回超で初めて needs-human。
#245 のセッション健全性判定を前提にした、その次の一歩。

- 受け入れ: 恒久沈黙 fixture で、人手なしに fresh spawn へ復帰し turn が完了すること。

#### B3. クラッシュループの circuit breaker + プロジェクト間のスロット公平性

- 同一 run の連続 PaneDied を数え、指数バックオフ(1分→5分→15分)、閾値超で run を
  park + `infra` 種別の集約通知(needs-human は使わない — ADR 0028 と同じ整理)。
- `max_concurrent_runs` に **per-project 上限**(例: グローバル10・各プロジェクト最大4)を
  足し、壊れた1プロジェクトが全スロットを食えないようにする。
- 受け入れ: FakeMux で即死を繰り返す profile を持つプロジェクトが、健全プロジェクトの
  同時実行数を減らさないこと。

#### B4. 「N open PRs resolve to issue」の沈黙 skip ループを廃止

決定的に1本選ぶ(meguri の branch 命名規則に一致する PR を優先、複数なら最新 head)。
選べない場合のみ、**dedup 付きで1回だけ** escalation(冪等化は #246 の枠組みに乗せる)。
毎 poll の run 行生成 + skip はやめる(1317 run 行はイベントログの信頼性も毀損する)。

#### B5. escalation 再駆動は既存 draft PR を adopt する(重複 PR の根絶)

§2-2 の 5(1 escalation = 重複 PR 1本)への対処。二層で塞ぐ:

1. **再駆動時の adopt**: planner / worker が issue を(再)claim する際、その issue に
   **自レール(meguri branch 命名規則に一致)の open PR** が既にあれば、新 branch を
   切らずその head branch を worktree に checkout して続きから駆動する。needs-human
   draft からの再開は「前回の成果 + 人間の裁定」を初期状態とする(台帳も checkpoint から
   引き継ぐ)。#253 のレール外ブロックと対になる、レール内の再利用規則。
2. **publish 時の dedup**: `publish_needs_human_draft` は新規 PR を開く前に同一 issue の
   自レール draft を探し、あれば **同じ PR に push + body 更新**で済ませる(escalation
   コメント dedup(#246)と同じ冪等化の思想)。

- 受け入れ: FakeForge で「escalate → 裁定 → 再駆動」を2周しても open PR が
  1本のままであること。#245 の実ケース(3本の重複)を fixture 化。
- 付随: spec が ADR を含む場合の番号は「main の次の空き番号」を publish 時点で
  再確認する(#257/#261 の ADR 0028 衝突の再発防止。docs ルールの運用を機械に守らせる)。

### Track C: 計測 — 効いたかを数字で閉じる

`meguri stats review` に追加(ADR 0026 の枠):

- ping-pong 率(escalation 中の ping-pong 型比率)と、finding kind 別・lens 別の内訳
- deferred 率(role/profile 別) + follow-up issue 起票数
- guard blocking の sustained 率(人間裁定で維持された率)、カテゴリ別
- stuck turn の自動 recover 回数 / escalate→resolve 滞留時間(先行設計 §5 の継承)
- プロジェクト別スロット占有時間(クラッシュループの可視化)

導入後 1〜2 週間で本書 §2 と同じクエリで再計測する。

## 5. 実施順序

1. **B1**(即・人間): #245 の重複 PR を1本に整理(#262 を残し #257/#261 を close)した上で
   f4 を裁定して着地 → stuck turn 対策の本丸が動き出す
2. **B4+B5**(小・独立): 沈黙 skip ループ廃止 + 再駆動 adopt — #246 の冪等化と同便で
3. **A2+A3**(核心): deferred disposition + ping-pong の decision 化。ADR 起草
   (0022 の台帳セマンティクス拡張 = 追記でなく新 ADR)
4. **B3**(小・独立): circuit breaker + per-project slot cap
5. **A1+A6**: closes_when + anchor 照合 + reviewer fresh session(ADR 同上に同梱可)
6. **A4**(config + 前提条件): #245 着地後に round-1 並列を有効化
7. **A5**: guard 被害シナリオ必須化(+ 稼働済み #248 の上で sustained 率を計測)
8. **C**: stats 拡張は 3〜7 の各スライスに計測を同梱する形で分割

依存関係: A2/A3 は ADR 0022 の改訂を伴うため ADR 先行。B2 は #245 の着地後。
それ以外は互いに独立で並行可能。

## 付録: 本書の主張を支えるクエリ

すべて `~/.meguri/meguri.sqlite` に対して:

```sql
-- ping-pong escalation の一覧
SELECT ts, data_json FROM events
 WHERE kind='escalation.raised' AND data_json LIKE '%ping-pong%';

-- guard verdict とカテゴリ(kind 別)
SELECT json_extract(data_json,'$.kind'), json_extract(data_json,'$.verdict'),
       json_extract(data_json,'$.categories'), COUNT(*)
 FROM events WHERE kind='pr_review.posted' AND ts>='2026-07-21' GROUP BY 1,2,3;

-- stuck turn(60分超の turn)
SELECT purpose, outcome, ROUND((julianday(finished_at)-julianday(started_at))*1440) m
 FROM turns WHERE m > 60 ORDER BY m DESC;

-- クラッシュループ(プロジェクト別 pane_died)
SELECT r.project_id, t.purpose, COUNT(*),
       ROUND(AVG((julianday(t.finished_at)-julianday(t.started_at))*1440),1)
 FROM turns t JOIN runs r ON r.id=t.run_id
 WHERE t.outcome='pane_died' AND t.started_at>='2026-07-21' GROUP BY 1,2;

-- 沈黙 skip ループ
SELECT error, COUNT(*) FROM runs WHERE status='skipped' AND error LIKE '%not picking one%'
 GROUP BY 1 ORDER BY 2 DESC;
```
