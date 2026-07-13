# ADR 0006: triage(意思決定の自動化)も read-only レポートから段階導入し、cleaner とは別レポート issue に分離する

## ステータス

採用(issue #85、triage v0)

## コンテキスト

discovery のトリガーは現状すべて人間のラベル付与に依存している(`flow::discover_by_label`)。
最終形は「人間が `meguri:ready` / `meguri:plan` を貼らなくても、meguri が open issue を自分で巡回して
トリアージし、着手対象と扱い方を自分で決めて回す」状態で、この最後の人手を自動化したい。

triage は cleaner(hygiene)より一段踏み込む。cleaner が自動化するのは**観測(検出)**だが、triage が
自動化するのは**意思決定**である。誤トリアージがそのままラベル → 自動着手に直結すると、間違った issue に
worker が張り付き PR まで走ってから気づく事故になる。「信頼のない自動判断ほど厄介なものはない」
(ADR-0003)は、判断の自動化においてなおさら重い。

一方で triage の出力は cleaner のレポートと同型(人間が読んで採用は既存フローへ、誤判定は無視リストへ、
停止は `meguri:hold` 1 枚)であり、cleaner が「掃引結果を人手で issue 化してね」と促していた最後の
手作業を triage が肩代わりしていく関係にある。両者を 1 本のレポート issue に同居させる案もあるが、
read-only 掃引(cleaner)と意思決定(triage)は責務が別で、本文の書き込み境界とマーカーが混ざる。

## 決定

1. **triage も read-only detector として導入する(ADR-0003 の踏襲)。** v0 の作用は「open issue の
   扱いを推薦する 1 本のレポート issue の作成・更新」のみ。他 issue へのラベル・コメントは一切しない
   (`meguri:working` claim も行わない)。書き込みへの昇格(v1 advise #87 の提案ラベル、v2 auto #88 の
   `meguri:ready`/`meguri:plan` 直接付与)は、信頼を積んだ後の別 issue とし、そのときの ADR で改めて
   昇格の是非を判断する。

2. **triage のレポート issue は cleaner とは別立てにする(`meguri:triage-report`)。** 責務が別
   (検出 ⇔ 意思決定)で、単一 issue に同居させると書き込み境界とマーカー(`meguri:clean` /
   `meguri:triage`)が混ざるため、「1 loop = 1 レポート issue」の型をそのまま二重化する。走査済み head・
   巡回時刻・走査時点の最大 open issue 番号はマーカーとして本文に埋め、スナップショットとして毎回
   全上書きする(Authority 原則)。

   再走査のトリガーは cleaner の「head 移動 + interval 経過」をそのまま流用しない。cleaner が見るのは
   コードと issue の乖離なので head の移動で律速できるが、triage の入力は open issue 集合であり、
   head だけを見ると「head 静止中に立った新規 issue を次の push まで拾わない」under-triage になる。
   よってマーカーの最大 issue 番号を超える open issue の出現もトリガーに加える(新規 issue の初回
   トリアージは head と独立に走る)。既存 issue の更新(updatedAt)に追従する再トリアージは v1 #87 の
   スコープ。

3. **トリアージ対象は「`meguri:` プレフィックスのラベルを 1 つも持たない open issue」で定義する。**
   「未ラベル = 未トリアージ」という ADR-0005 の不変条件をそのまま discovery 条件に使う。個別ラベルの
   列挙ではなくプレフィックス判定にすることで、ワークフローラベル・レポートラベル・将来の提案ラベルを
   まとめて対象外にでき、triage が自身や cleaner のレポート issue を再トリアージする事故も防げる。
   加えて `meguri:hold`・未解決 blocker の issue も除外する。

4. **既定 off の完全オプトイン。** cleaner は常駐だが、triage は判断寄りでリスクが一段高いため、
   `[triage] mode = "off"` を既定とし、明示的に `report` を選んだときだけ巡回する。

## 帰結

- 誤トリアージが壊すものが無い(ラベルも着手も動かさない)ため、escalation や bot ループ防止機構なしで
  安全に導入できる。失敗は静かにスキップして次回巡回に委ねられる(cleaner と同じ)。
- 人間の操作面が 1 本の issue に集約される: 採用は自分で `meguri:ready`/`meguri:plan` を貼って既存
  ループへ、誤判定は `triage.ignore` へ、停止は `meguri:hold` 1 枚。
- cleaner と triage という near-identical な read-only レポートループが 2 本並ぶが、責務(検出 ⇔ 意思決定)
  と書き込み境界(それぞれ専用レポート issue)で明確に分かれる。
- 将来 advise / auto へ昇格する際も、この issue が推薦の供給源になる。昇格に伴う重い機構
  (confidence 閾値・レート制限・可逆性・人間ラベル非上書き)は v1 #87 / v2 #88 の ADR で扱う。
