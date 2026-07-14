# ADR 0015: triage advise(v1)は提案ラベル + 根拠コメントで持ち出し、内容ハッシュの hidden マーカーで冪等性を担保する

## ステータス

採用(issue #87、triage v1 advise)

## コンテキスト

triage v0(ADR 0006、issue #85)は read-only で、推薦を 1 本のレポート issue にまとめるだけだった。
人間は毎回レポートを読み、該当 issue に自分で `meguri:ready`/`meguri:plan` を貼る必要がある。
ADR 0006 は「昇格に伴う重い機構(confidence 閾値・レート制限・可逆性・人間ラベル非上書き)は v1/v2 の
ADR で扱う」と留保していた。本 issue(v1 advise)がその一段目で、推薦を各対象 issue の面へ持ち出す
——ただし worker/planner はまだ起動しない。

3 つの実装上の問題を先に決める必要があった。

1. **discovery を汚さない安全策。** 提案ラベル(`meguri:triage-ready` 等)は `meguri:` プレフィックス
   を持つが、worker/planner の discovery は `LabelTaskSource::discover` が `meguri:ready`/`meguri:plan`
   を forge の `list_issues_with_label` で厳密一致検索する(プレフィックス走査ではない)ため、
   提案ラベルを本ラベルと取り違えて着手が走ることは元々ない。
2. **再トリアージの対象。** triage 自身の候補集め(`gather_candidates`)は「`meguri:` ラベルを 1 つも
   持たない = 未トリアージ」で候補を絞る(ADR 0006 決定 3)。提案ラベルもこの判定に含めると、一度
   提案した issue は内容が変わっても二度と候補に戻らず、「内容が変わった issue だけ再トリアージ」
   (ADR 0006 が v1 に残した TODO)が実現できない。
3. **冪等性と却下の尊重の両立。** 「同じ推薦を再コメントしない」ことと「人間が提案ラベルを剥がしたら
   再提案しない」ことは、ローカル状態を持たない前提(Authority 原則)では同じ仕組みで解く必要がある
   ——ローカル DB に「この issue には提案済み」を覚えさせると、再起動やホスト移動で失われる。

## 決定

1. **提案ラベルは discovery の「実ラベル」から明確に除外する。** `src/forge/mod.rs` に
   `TRIAGE_PROPOSAL_LABELS`(`meguri:triage-ready`/`-plan`/`-needs-human`)を定義し、triage の
   `gather_candidates` は「`meguri:` プレフィックスを持ち、かつ `TRIAGE_PROPOSAL_LABELS` に含まれない」
   ラベルだけを「エンゲージ済み」とみなす(`is_engaged_label`)。worker/planner discovery 側は元々
   厳密一致なので変更不要。

2. **再トリアージは内容ハッシュで判定する——discovery そのものの再走査トリガーとしても。** 提案
   ラベルだけを持つ issue は、`(title, body)` の SHA-256(reconcile ループの `tasks::body_digest` を
   流用、issue #142)が前回の提案時と変わっていれば候補に戻る。ハッシュは issue の updatedAt より
   頑丈(ラベル操作だけの更新に反応しない)で、ローカル状態も要らない。ただし `gather_candidates` の
   フィルタだけでは足りない——`TriageLoop::discover`/`prepare_work` の再走査判定(`needs_triage_scan`)
   は元々「head 移動」「新規 issue」の 2 signal しか見ておらず、提案済み issue の内容だけが変わって
   push も新規 issue も無いケースでは、そもそもスイープ自体が起動せず `gather_candidates` まで
   到達しない。そこで `needs_triage_scan` に 3 つ目の signal(`advise_content_changed`)を足し、
   `advise` モードでは「実ワークフローラベルを持たない」open issue を走査して既存マーカーとの
   ハッシュ差分を見る(`advise_backlog_changed`/`marker_drifted`)。**提案ラベルの有無では絞り込まない**
   ——人間が提案を却下してラベルを外しても根拠コメントの hidden マーカーは残るので、却下後に本文が
   編集されたケースもここで拾う(ラベルで絞ると却下済み issue の再編集を見逃す)。マーカーが無い
   issue(一度も提案していない、普通の未トリアージ issue)は drift 扱いにしない——さもないと
   「head 静止・新規issue無しなら再走査しない」という不変条件を壊してしまう。この signal も他の
   2 つと同じく `interval_hours` でレート制限される。コストが伴う(open issue 全件 + 各 issue の
   `issue_comments` 呼び出し)ため、head/新規 issue の 2 signal で足りる場合はこの走査自体をスキップ
   する(遅延評価)。

3. **冪等性・却下尊重の両方を、根拠コメントに埋めた 1 個の hidden マーカーで解く。** 根拠コメント本文
   の先頭に `<!-- meguri:triage-advise hash=<sha256> recommendation=<rec> -->` を埋め、書き込み前に
   その issue の全コメントから最新のマーカーを読み直す(`Forge::issue_comments`、`pr_comments` の
   issue 版として新設)。マーカーのハッシュが現在の内容ハッシュと一致すれば、ラベルの有無に関わらず
   何もしない——一致は「まだ人間が判断していない(ラベルは残っている)」か「却下された(ラベルを
   剥がされた)」のどちらかで、いずれも内容が動かない限り再提案する理由がないため。不一致(内容が
   変わった)なら、古い提案ラベルを外し新しい推薦のラベルを付け直し、新しいハッシュで再コメントする。
   状態はすべて forge 上(issue 自身)にあり、ローカル DB には何も残さない(Authority 原則)。

4. **書き込みは `ready`/`plan`/`needs-human` の 3 推薦だけに限定する。** `hold`/`skip` は提案ラベルを
   持たず(`proposal_label` が `None` を返す)、レポートには載るが対象 issue には何も書かない——
   「保留・対象外」は人間の判断を仰ぐほどの提案ではないため。

5. **`triage.max_actions_per_tick`(既定 3)で 1 tick の書き込みを律速する。** 予算切れの issue は
   個別マーカーが更新されないため、次回スイープでも候補のまま残り続ける——ただしそれだけでは
   「次回スイープが実際に起動する」保証にならない。レポート issue のマーカーは head/max_issue を
   毎回現在値へ進めるので、予算切れの残件があっても他に何も変化がなければ discovery の 3 signal
   全てが静かなまま、という事故が起きうる。そこでレポートマーカーに 4 つ目のフィールド
   `backlog=0|1` を追加し(`triage_marker`/`parse_triage_marker`、旧マーカーは `backlog=0` として
   後方互換に解釈)、`apply_advise` が予算切れ・書き込み失敗で処理しきれなかった actionable な
   推薦を残した回は `backlog=1` を記録する。`needs_triage_scan` はこれも「変化」の1つとして数え、
   次回 `interval_hours` 経過後に必ず再スイープを起動する——「残りは次回に繰り越す」が名実ともに
   成立する。

6. **書き込みの成否は個々に閉じる(ベストエフォート)。** 1 issue への提案が失敗しても(close race、
   forge の一時エラー等)スイープ全体は失敗させず、警告ログを残して次の issue に進む。レポート発行
   と走査マーカーの前進は従来どおり毎回行われる。

## 帰結

- 人間の操作が「レポートを読んで手でラベルを貼る」から「提案ラベル + コメントを見て承認 or 剥がす」
  に縮む。誤トリアージのコストは相変わらず低い——提案ラベルは worker/planner discovery の外にあり、
  本ラベルへの昇格は常に人間の手作業。
  自動着手は依然として起きない。
- `Forge` トレイトに `issue_comments` が増え、`gh.rs`/`fake.rs` 両実装が対応。他の実装は無い
  (2026-07 時点)。
- v0 のレポート issue は advise モードでも変わらず発行され、フッターの文言だけが「提案ラベル/コメント
  も付けている」旨に切り替わる——レポートは常に全体のスナップショットであり続ける。
- v2 auto(#88)は、この ADR の「内容ハッシュ + hidden マーカー」idempotency をそのまま流用しつつ、
  書き込み先を提案ラベルから本ラベルに変えるだけで済む見込み。
