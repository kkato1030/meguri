# ADR 0007: merge-watch は fixer ループに委譲し、どのループも拾わない stall だけをエスカレーションする — watch はドリフト検出であってマージ権威ではない

- Status: proposed
- Date: 2026-07-12
- Issue: #42

## Context

ADR-0003 で meguri は「マージして安全か」を自前で判定せず、GitHub ネイティブ auto-merge に arm するだけと決めた。その帰結として ADR-0003 は「arm 後に条件が崩れた場合(conflict / red CI / protection 変更 / 人間による解除)のドリフト検出・解除は後段の merge-watch(別 issue)で扱う」と明記していた。#42 がその merge-watch。looper も同じ問題を ADR-0005「watch is drift detection, not merge authority」で扱っており、meguri も同じ立場に立つ。

問題は、#42 が起票された時点の前提が現在の main では進んでいることだ。merge-watch は当初「conflict / red CI を検出したら、専用の解消ループができるまでの当座しのぎとして `meguri:needs-human` を貼って回送する」設計だった。しかしその「回送先」である 2 つの常駐ループが、この issue の起票後にすでに landed している:

- **conflict-resolver ループ(#35, CLOSED)** — arm の有無に関係なく、open な meguri PR で `mergeable == CONFLICTING` のものを discover し、ベースを取り込んで解消 push し、解消不能なら自ら `meguri:needs-human` にする。
- **ci-fixer ループ** — open な meguri PR の check rollup が `FAILURE` のものを discover し、失敗を修正 push し、予算超過で自ら `meguri:needs-human` にする。

さらに #41 の auto-merger のマーカー(`<!-- meguri:automerge armed head=<sha> -->`)は、既に「人間が auto-merge を無効化した head には再 arm しない」を冪等キーとして保証している。

この 3 つが揃った今、#42 を額面どおり(conflict / red CI に needs-human を貼る)実装すると、単に重複するだけでなく **conflict-resolver も ci-fixer も `meguri:needs-human` 付き PR を discover から除外する**ため、機械的に直せるドリフトを **永久にデッドロックさせる**。どこまでを merge-watch の責務にするかを固定する必要がある。

## Decision

1. **merge-watch は fixer ループが拾うドリフトには一切介入しない(委譲)。** `CONFLICTING` は conflict-resolver、required check の失敗(`mergeStateStatus == BLOCKED` かつ rollup `FAILURE`)は ci-fixer が既に arm と無関係に拾う。merge-watch はこれらを分類はするが **no-op** にする。とりわけ **needs-human を貼らない** — 貼れば当の fixer ループを締め出してデッドロックさせるからだ。それらのループが直せなければ、ループ自身が needs-human にエスカレーションする(責務の一本化)。

2. **merge-watch が固有にエスカレーションするのは「どのループも拾わないまま放置された arm 済み PR(Stuck)」だけ。** 例: branch protection に後から required check が追加され workflow 側に存在しない → その check は永久に走らず PR は `BLOCKED`、だが conflict でも rollup `FAILURE` でもないので conflict-resolver も ci-fixer も拾わない → 誰にも気づかれず放置される。これが #35 と同じ放置問題の、専用ループでは塞げない最後の穴であり、merge-watch はその backstop。arm 済みが長時間どの担い手も無く止まっていれば `meguri:needs-human` + コメントで人間に返す。

3. **人間が auto-merge を無効化した PR(HumanDisabled)には黙って手を引く。** 再 arm もコメントもエスカレーションもしない。人間の決定が最終(ADR-0003 のマーカー原則の延長)。

4. **「required checks のみを数える」は GitHub の `mergeStateStatus` に委ねる。** required かどうかを meguri が branch protection の required check 名を列挙して自前判定すると、GitHub 側の設定変更に判定が置いていかれる(ADR-0003 が禁じた二重判定の再来)。代わりに GitHub の判定をそのまま使う: required でない check の失敗は GitHub が `UNSTABLE`(マージ可能)を返すので触らない、required check の失敗は `BLOCKED` を返すので RedCI として扱う。required の権威は GitHub にあり、meguri は再導出しない。

5. **watch 状態はローカルにも専用マーカーにも持たず、既存の forge データから毎掃引導出する。** 必要な状態は「いつから watch しているか(#41 arm マーカーコメントの `createdAt`)」「今どうなっているか(ライブの `mergeStateStatus` / `autoMergeRequest` / rollup)」「エスカレーション済みか(`meguri:needs-human` ラベル)」の 3 つで、すべて forge 上にある。専用の `meguri:merge-watch` マーカー(コメント upsert)を新設しない。TransientError(429/5xx)も別勘定にせず、状態が取れないまま放置閾値を超えたら Stuck に畳む。これにより sqlite に一切依存せず、meguri をいつ kill しても forge から復旧できる(Authority 原則)。

## Consequences

- merge-watch の実装表面積は小さい。掃引は run/pane を持たない軽 API 掃引(reaper / auto-merger と同型)で、副作用は Stuck 時の 1 ラベル + 1 コメントだけ。分類は純関数で単体テストできる。
- ドリフトの回送先が責務ごとに一本化される: conflict は conflict-resolver、red CI は ci-fixer、どのループも拾わない stall は merge-watch、人間の無効化は誰も触らない。エスカレーション経路が二重化しないので、同じ PR に複数ループが needs-human を取り合う競合が起きない。
- Stuck の判定は arm-since 起点の壁時計閾値なので、arm 済みで人間レビュー待ちが長引いた PR も(それ自体が放置リスクなので)nudge される。閾値が短すぎるとレビュー待ちの正常な PR を急かすノイズになるため、まず保守的な既定(24h 目安)で始め、必要なら config 化する。
- GitHub の `mergeStateStatus` に required 判定を委ねるため、rulesets 運用や `mergeStateStatus` が `UNKNOWN`(GitHub が計算中)を返す間は、その掃引では判定を保留し次掃引で再試行する(reaper / conflict-resolver の `Unknown` 扱いと同じ)。
- 専用マーカーを持たないため、「各 arm 済み PR の watch 状態」を外部(`meguri top` 等)から一覧する用途は今は無い。必要になれば marker 導入で追加できるが、その時に別途決める。
