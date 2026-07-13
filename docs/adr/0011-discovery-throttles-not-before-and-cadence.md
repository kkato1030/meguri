# ADR 0011: discovery に2つの調速機構(not-before / 消化レート上限)を足す — 時刻状態は forge に持たず、消化実績はローカル run 履歴で数える

- Status: proposed
- Date: 2026-07-14
- Issue: #148

## 文脈

meguri の discovery はキューにある actionable なタスクを(並列上限の範囲で)即座に消化する。`LabelTaskSource` / `LocalTaskSource` の `discover` は「トリガーラベル or queued 状態」「hold / working でない」「未 ship」「未解決ブロッカーが無い」を満たした issue/タスクをそのまま `Task` として返し、scheduler が run を立てる(`src/tasks.rs`, `src/engine/scheduler.rs`)。この設計には**時刻に縛られたタスクをキュー駆動で表現する術がない**:

- 「この日時以降に着手してよい」— 公開解禁日のあるコンテンツ、外部イベント待ち(not-before)。
- 「この種類のタスクは一定期間に N 件まで」— 媒体のレート/コスト制約で「SNS 投稿は1日1本」と決まっている運用(消化レート上限)。

現状の回避策は「人間または外部 cron が小出しにトリガーラベルを貼る」で、キューの一括編成(週次でまとめて起票し、あとは meguri に任せる)ができない。#146(1/2, ADR 0009)で「起票の入口」を時刻駆動化したのと対になる形で、「消化の調速」を discovery に足す。この2つで、外部 cron なしの時刻駆動運用が入口(起票)と出口(消化)の両輪で完成する。

具体的な動機は ai-revenue-engine の運用移行検討である。週次でキューを issue として一括編成する設計にしたところ、日次ペース必須のタスク(SNS 投稿)だけがキュー駆動に載せられず外部 cron に残った。

## 決定

**discovery(`src/tasks.rs`)の同じ層に、claim より前・dependencies チェックと同格の2つの調速ゲートを足す。ゲートに引っかかったタスクは forge 側に痕跡を残さずサイレントにスキップし、可視化はローカル CLI が担う。**

### 1. not-before — 「この日時までは discover しない」

- **github mode**: issue 本文の hidden マーカー `<!-- meguri:not-before <TS> -->` を discover が読む。マーカー表現は cleaner の head-sha マーカー / #146 の schedule マーカーと同じ「本文 hidden コメント」流儀に揃える。**ラベル(`meguri:after-…`)では表現しない** — 解禁日の数だけラベルが増えてラベル爆発を起こし、二軸ラベル(ADR 0005)の一義性を壊すからだ。
- **local mode**: `tasks` 行のフィールド(`not_before` カラム)として持ち、`meguri add --not-before <TS>` で設定する。ローカルタスクは本文マーカーではなく構造化フィールドが自然。
- 時刻前の issue/タスクは discover の戻り値から外れる。**ラベルもコメントも書かない** — GitHub-native dependencies のブロック時(`has_unresolved_blockers`)と同じ流儀。

### 2. 消化レート上限(cadence) — 「この期間の消化実績が上限なら discover しない」

- config `[[projects.cadence]]`(`[[projects.schedules]]` と同じ array-of-tables の並び)で、ラベル → 期間あたりの上限を宣言する。`label`(例: `sns`)と、期間モードとして `max_per_day`(暦日あたり)**または** `per_hours` + `max`(ローリング窓)のどちらか一方を持つ。
- **消化実績はローカル sqlite の run 履歴で数える。github のラベル・コメントには一切書かない。** discover は「窓内にこの cadence バケツで作られた run 数」を数え、`上限 - 消化数` を超える分のそのバケツのタスクをスキップする。
- cadence は **github mode(issue のラベル)専用**とする。local タスクには任意ラベルの分類軸が無いため(`kind` = work/plan だけ)、local mode の cadence は v1 スコープ外。

## この決定の根拠

1. **Authority 原則(forge が唯一の永続 workflow 状態)と整合する。** ラベルは「誰の番か・どのフェーズか」という workflow 状態を表し(ADR 0005)、**実行の記録(いつ・何件消化したか)はローカル**に置く — これは #146 の `schedule_state`(最終発火時刻を sqlite に置く)や cleaner の interval と同じ立場だ。消化実績を GitHub コメントやラベルで表現すると、forge が「実行ログ」を持ち始めてこの分離が崩れる。single-host 前提(local mode / `schedule_state` と同じ)で、Phase 4 の remote TaskSource(ADR 0003)に載る際に、消化カウントの権威をどこに置くかは再考する。

2. **サイレントスキップが dependencies と同型で、既存の収束機構をそのまま使える。** not-before / cadence は「今はまだ actionable でない」を表すのであって、失敗でもエスカレーションでもない。`has_unresolved_blockers` が「open なブロッカーがある間はラベルもコメントも足さず discover から外す」のと寸分違わぬ振る舞いにすることで、時刻が通過した瞬間・窓がロールオーバーした瞬間に、何の後始末もなく自然に discover に載る。ラベルやコメントで「待機中」を表現すると、通過後にそれを剥がす別処理と、剥がし忘れのデッドロック(ADR 0007 が踏んだ罠)を新たに抱える。

3. **消化実績を run 履歴から数えるのは、run 作成が消化の唯一の証跡だから。** discover は毎 tick 呼ばれ、並列上限で dispatch されないこともある読み取り的操作なので、discover 自身にカウンタを増やさせると過大計上する。「実際に run が立った」ことだけが消化の真実であり、それは既に `runs` テーブルに1行ある。run に cadence バケツを刻んで(`runs.cadence_label`)窓内 COUNT すれば、カウントは冪等で、restart をまたいでも正しい。

## 消化カウントの不変条件(実装が満たし続けるべき規則)

- **消化 = 窓内に立った run のうち `skipped` でないもの。** `Skipped`(discover と claim の間で誰かに取られた benign race)は実際には何も触っていないので数えない。それ以外(`succeeded` / `failed` / `running` / …)は**成否によらず1消化**と数える — 用途が「媒体のレート/コスト制約」である以上、失敗した SNS 投稿を無制限にリトライして枠を食い潰さないほうが正しい。「1日1本」は「1日1回の試行」を意味する。
- **窓の定義**: `max_per_day` は暦日 `[今日の 00:00, now]`、`per_hours = H` はローリング `[now - H*3600, now]`。**v1 の暦日境界は UTC**(#146 の cron / schedule が UTC-only で timezone を deferred したのと同じ立場)。UTC 深夜のロールオーバーが運用上ずれる場合は `per_hours`(タイムゾーンに依らないローリング窓)を使えば回避できる。設定可能な UTC オフセットは #146 と同様に将来課題とする。
- **cadence バケツは run 作成時に確定させ、以後不変。** discover が「この issue はラベル `L` の cadence 対象」と判定した事実を run に刻む。後から issue のラベルが変わっても、過去の消化実績は動かない。
- **enforcement は discover 呼び出し単位。** cross-kind(work と plan)で同一ラベルの issue が同 tick に別々の discover 呼び出しで消化されると、瞬間的に上限を +1 超えうる。用途(「1日1本」の運用制約、通常は `ready` 直行)ではこの緩みは無害なので v1 は許容し、CLI 可視化と次 tick のカウントで収束させる。

## not-before マーカー不正時の扱い

not-before マーカー / フィールドが解析不能(タイポ等)なとき、**fail-open(ゲート無しとして即 discover)ではなく fail-closed(通さない)** とする。用途が「公開解禁日」である以上、日付のタイポで解禁前に公開してしまう事故のほうが、解禁が遅れる事故より重い。fail-closed で無期限に止まっても、`meguri tasks` が「not-before 待ち(解析不能なマーカー)」として理由付きで見せるため、サイレントに詰まることはない。

## 帰結

- discover ゲートの順序は `hold/working` → 未 ship → **not-before** → **cadence** → dependencies と同じ層に並ぶ(claim より前)。
- `runs` に `cadence_label`、`tasks` に `not_before` カラムが増える(migration 追加)。`Target` に cadence バケツを載せ、scheduler が run 作成時に刻む。
- discover に「現在時刻」を注入する必要が出る(fake clock でのテストのため)。TaskSource 実装は注入可能な epoch clock を保持する(既定はシステム時刻)。
- サイレントスキップは forge に痕跡を残さないため、**CLI 側で見えることを必須要件**とする(`meguri tasks`)。可視化は discover と同じゲート関数を読み取り専用で回して算出し、discover と表示がドリフトしない単一実装にする。
- 「起票の入口」を扱う #146(1/2, ADR 0009)と、この「消化の調速」を扱う #148(2/2)は入口と出口で対になる。両者とも消化パイプライン(worker/planner のループ・claim・エスカレーション・完了)そのものは置き換えない。
