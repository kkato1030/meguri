# ADR 0017: triage auto(v2)は閾値超えの推薦を本ラベルへ昇格し自動着手する — 多重ガードと理由コメントで可逆に留める

## ステータス

採用(issue #88、triage v2 auto)

## コンテキスト

triage は read-only から段階昇格してきた。v0(ADR 0006、#85)は 1 本のレポート issue に
推薦を集めるだけ。v1(ADR 0015、#87)は各 issue に提案ラベル(`meguri:triage-*`)+ 根拠
コメントを持ち出したが、本ラベルは付けない — 昇格は必ず人間の手作業だった。

v2 はその最後の一手を自動化する。閾値を超えた推薦を **本ラベル(`meguri:ready` /
`meguri:plan` / `meguri:needs-human`)として直接付与**し、既存の worker / planner ループへ
自動投入する。ここで初めて discovery の人手依存が外れる。

ADR 0003(cleaner)は「昇格の是非・confidence 階層・bot ループ防止はそのときの ADR で
改めて判断する」と留保していた。本 ADR がその判断である。read-only の v0/v1 と違い、v2 は
**誤トリアージが PR まで直結する**。防御機構を揃えてからでないと入れられない。

## 決定

### 1. 徹底したオプトイン

`[triage] mode` の既定は `off`。`auto` は明示設定でのみ動く。cleaner は常駐するが、triage は
「意思決定」を自動化するので、頼まれるまで黙っている。

### 2. 昇格は2つの config で絞る

- `confidence_threshold`(既定 `0.7`): エージェントの自己申告 confidence がこの値以上の推薦
  だけを昇格対象にする。0.7 は「まず着手して大きく外さない」と読める下限として置く保守値で、
  運用ログを見ながら上げ下げする。全昇格の共通ゲート。
- `apply`(既定 `["ready"]`): どの推薦種別を昇格するか。認識する値は `ready` → `meguri:ready`、
  `plan` → `meguri:plan`、`needs-human` → `meguri:needs-human`。`hold` / `skip` は昇格対象に
  なる本ラベルを持たない(v1 の `proposal_label` が `None` を返すのと同じ)。

**既定を `["ready"]` にする理由。** issue は既定案として `["ready", "plan"]` を挙げつつ、段階的
ロールアウトとして「まず `ready`(小スコープの明確な issue)から始め、`plan` は planner の消費が
大きいので信頼を積んでから」と述べている。この2つは両立しない — 既定が広いと、安全な出発点へ
入るのに操作者が明示的に狭める必要が生じる。既定はいつも安全側であるべきなので、shipped default
は `["ready"]` とし、`plan` の自動投入は操作者が信頼を積んでから `apply` に足す。

### 3. 昇格は必ず可逆に:理由コメント + 冪等マーカー

- **理由コメント必須。** 昇格のたびに、何を・なぜ・どの confidence で付けたか、そして「本ラベルを
  剥がすだけで差し戻せる」旨を1件コメントする。人間はコメントを読んで即座に剥がせる(可逆性)。
- **冪等・却下尊重は v1 のマーカーを流用**。根拠/理由コメント先頭の hidden マーカーに、昇格が
  かかった時点の内容ハッシュと **適用レベル**(提案どまり=proposal か 本ラベル昇格=real か)を
  記録する。auto 昇格は「最新マーカーが現在の内容ハッシュと一致し、かつ既に real 昇格済み」の
  ときだけスキップする。これで2つを同時に解く:
  - **advise → auto の移行**: 提案どまり(proposal)の古いマーカーが同じハッシュで残っていても、
    auto は real 昇格へ進む(提案と昇格は別物)。
  - **却下の尊重**: 人間が本ラベルを剥がした後、内容が変わらない限り再昇格しない(real マーカーが
    現ハッシュと一致 → 無操作)。内容が動いたら剥がれた本ラベル/提案ラベルを外し、新しい推薦で
    貼り直す。ローカル状態は持たない(Authority 原則)。

### 4. bot ループ防止は「本ラベルそのもの」で足りる

triage が `meguri:ready` を付けた瞬間、その issue は本ワークフローラベルを持つ = triage の
`is_engaged_label` が真になり、候補集め(`gather_candidates`)から外れる。worker が着手して
`meguri:working` へ進めば以降も engaged のまま、完了すれば close する。**昇格済みを除外する
「マーカー」は本ラベル自身であり、別立ての印は要らない**。理由コメントのマーカーは
「却下後の再昇格を止める」ためだけにある。`needs-human` 昇格(`meguri:needs-human`)も同じく
engaged なので再トリアージされない。

### 5. autonomy 境界(何を自動で始めてよいか)

- `ready` 昇格 → worker が spec なしで実装に着手してよい。
- `plan` 昇格 → planner が spec を書き始めてよい。ただし消費が大きいので既定 `apply` からは外し、
  操作者が明示投入する。
- `needs-human` 昇格 → **着手ではない**。worker/planner は `meguri:needs-human` を discovery
  しない。人間に球を渡すだけの、v1 の提案ラベルと同程度に低リスクな routing。
- triage は **本ラベルの付与までしかしない**。PR を開く/マージするのは既存ループの責務で、
  triage の書き込み境界ではない。
- 閾値未満・`apply` 外・`skip` / `hold` は **据え置き**(per-issue の書き込みなし。v1 の提案
  ラベルが既に付いていればそのまま)。auto は advise の提案を重ねて出さない。

### 6. レート制限で被害を有界化

`triage.max_actions_per_tick`(既定 3、v1 から流用)で 1 tick の本ラベル付与数を厳格に上限する。
暴走時の被害を1スイープぶんに閉じ込める。使い切って積み残した昇格可能な推薦はレポートマーカーの
`backlog=1` に記録し、次スイープを必ず起動する(v1 の仕組みをそのまま使う)。

### 7. 監査

昇格ごとに `triage.promoted` イベント(issue / recommendation / label / confidence)を events に
記録する。`meguri logs triage.promoted` で「triage が何を着手させたか」を後追いできる。

## 帰結

- discovery の人手依存が初めて外れる。人間の操作は「本ラベルを剥がして差し戻す」「`triage.ignore`
  で黙らせる」「`meguri:hold` でスイープを止める」に縮む。
- **可逆性は polling とのレースを伴う**。triage が本ラベルを付けてから worker が discovery で
  拾うまでの間に人間が剥がせば着手は走らない。拾われた後(既に `meguri:working`)は、本ラベルを
  剥がしても worker は止まらない — その場合は run を stop するか `meguri:hold` を使う。可逆性は
  「polling 1 周期ぶんのベストエフォート」であり、この窓を rate limit と interval が縛る。
- ロールバック手順:(a) 単一の誤昇格 → 本ラベルを剥がす(未着手なら差し戻し完了、着手済みなら
  run を stop)。(b) auto 全体を止める → `mode` を `advise` か `off` に戻す(次スイープから昇格
  しない。既に付いた本ラベルは各 issue で剥がす)。(c) 特定パターンの誤検知 → `triage.ignore`。
- 段階的ロールアウト:`apply = ["ready"]` で小スコープの明確な issue だけを自動着手させ、ログで
  誤トリアージ率を見る → 信頼が積めたら `["ready", "plan"]` へ広げる。confidence_threshold も
  同様にログを見て調整する。
- v1 の「内容ハッシュ + hidden マーカー」idempotency をほぼそのまま流用できた。書き込み先を提案
  ラベルから本ラベルへ変え、マーカーに適用レベルを1つ足しただけ(ADR 0015 の見立てどおり)。
