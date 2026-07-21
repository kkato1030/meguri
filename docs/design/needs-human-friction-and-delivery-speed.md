# 設計書: needs-human 摩擦の実測と、PR デリバリを速くするための改善

- Date: 2026-07-21
- Status: 提案(個別改善は issue 化して個別に判断する)
- 調査範囲: 2026-07-20 12:00 〜 2026-07-21 12:40 JST、meguri プロジェクトの
  open issue 全件(#222 #223 #224 #225 #232 #234 #235 #236)を人間ゲート付きで
  マージまで走らせた実運用ラン。データ源は events テーブル・PR コメント・pane 実画面。

## 1. なぜ書くか

needs-human は「人間にボールを渡す」設計の要だが、**渡し方が悪いと人間が律速になる**。
今回のランで needs-human・awaiting_human が何回・なぜ起きたかを全件追跡し、
「人間でなくても処理できたもの」「そもそも起きるべきでなかったもの」を切り分けた。

## 2. 実測 — 何が何回起きたか

観測窓内の meguri プロジェクトのエスカレーション(escalation.raised / turn.awaiting_human):

| 時刻(UTC) | 対象 | 表向きの理由 | 真因(調査後) |
|---|---|---|---|
| 07-20 22:29 | PR #230, #231 | レビュー未完了 | run が profile `gpt` に pin されたまま config から一時消えた(設定編集ドリフト) |
| 07-21 00:50, 01:37, 03:06 | PR #231 | spec 改訂3回でも plan review 赤 | ①reviewer が**解消済み finding を再主張**(下記 3-B)。②同一エスカレーションが**3回重複発火**(下記 3-C) |
| 07-21 02:25 | PR #238 | guard(impl) findings | 正当な指摘2件(ADR の定義漏れ)。ただし自動 fix 経路が無く即 needs-human |
| 07-21 03:02, 03:31 | PR #239 | guard(impl) blocking | 正当な指摘(doctor の無期限 wait / orphan 蓄積)。ただし**1回のレビューで1件ずつ**しか出ず、2往復かかった |
| 07-20 13:31 〜 07-21 03:14 | #222/#223 turn ×7 | agent_quiet | gpt pane が **context 100% + API 400 で恒久沈黙**。resume が同じ死にセッションへ戻り続け、nudge→escalate→recover を数時間ループ(下記 3-A) |
| 07-20〜21 ×3 | #222 #232 #234 turn | runtime_budget_exceeded | 長時間ターン。正当な tripwire だが、その後の再開は人間頼み |

参考(観測窓の外、07-14 以降の全プロジェクト集計): self-review cap 不収束 53 件、
PR レベル escalation 36 件、herdr ConnectionRefused の重複 escalation 23 件。

**人間ゲートの裁定結果**: 実質的に人間の判断が必要だったのは
「#238/#239 の guard finding を採るか」の2件だけ。それ以外は
(a) 起きるべきでない機械的故障、(b) 機械照合で棄却できた stale finding、
(c) 自動 fix 1周で解消できた軽微指摘、のいずれかだった。

## 3. 真因の分類

### 3-A. 復旧不能セッションの resume ループ(最大の時間損失)

> 再発記録: 本ドキュメント起票の数時間後(07-21 06:14 UTC)、別 issue(#235)の
> pr-review lane でも同一故障が再発した(gpt reviewer が spec レビュー3周を同一
> セッションで回して context 100% 到達)。単発事故ではなく、review 系 lane を
> resume で回し続ける構造そのものが原因である傍証。

gpt プロファイル(cliproxyapi 経由・context window が claude より小さい)の
pr-reviewer セッションが context 100% に達し、以後すべての入力が
`API Error: 400 input exceeds the context window` になった。transcript は 8.8MB。

- `ensure_pane` は pane 行の `agent_session_id` を無条件に resume する。
  `resumed_pane_survives` は「即死」しか検出しないため、**開くが全メッセージ 400**
  のセッションは健常と誤認される。
- セッションのクリアは `TurnOutcome::PaneDied`(resume 後に pane 死亡)のみ
  (`src/engine/flow.rs` `record_agent_session`)。pane が生きたまま無応答のケースに
  クリア経路が無い。
- 結果: nudge×2 → awaiting_human(agent_quiet) → crash recovery → 同一セッション
  resume → 400、を **13時間以上**ループ。escalation は出るが、理由が generic な
  `agent_quiet` のため人間が pane を開くまで真因が分からない。
- 派生症状: 人間が transcript を退避して復旧を試みた後も pane 行の session id が
  残るため、次 run が同じ id を resume → claude が即終了して **pane は素の zsh に
  落ちる**。以後の nudge はシェルプロンプトに打ち込まれ(`zsh: command not found:
  otherwise`)、pane は生きているが agent は不在 — **pane 生存 ≠ agent 生存**を
  現行の quiet 検出は区別できない(mux の `AgentState` に判定材料はある)。

### 3-B. レビューが「直っているものを直っていない」と言う(stale finding)

PR #231 の最終 blocking finding は「spec に存在しない `stopped` status が残っている」
だったが、**レビュー対象 head(816dbe3)にその文字列は存在しない**(前 head への指摘の
再主張)。pr-reviewer は resume されたセッションで動くため、旧 head の記憶が
現ファイルの読み直しに勝ってしまう。この1件で spec_fixer の budget(3)が空転し、
needs-human に落ちた — つまり **偽の不収束**。

同型の逆パターンとして、#239 では guard が正当な finding を**1レビュー1件ずつ**
しか出さず(1周目: 無期限 wait、2周目: error 経路の orphan)、往復数が膨らんだ。

### 3-C. エスカレーションが冪等でない

PR #231 に同一文面の needs-human コメントが 00:50 / 01:37 / 03:06 の3回付いた。
`spec_fixer::escalate_budget_exhausted` は `let _ = add_pr_label(...)` で
書き込み結果を捨て、次 sweep の抑止を「ラベルが付いたはず」という仮定に預けている。
ラベル書き込みの失敗(または informer cache の stale 読み)で仮定が崩れると、
level-triggered ループがそのまま重複発火する。イベント上、ラベルを外した actor は
存在しない — 書いたつもりが書けていなかった、が最有力。

### 3-D. レール外デリバリとの重複

issue #236 に対し、レール外ブランチ(`adr/0026-review-efficacy`)の手書き spec PR #237
(spec-ready ラベル付き)と、meguri の worker が開いた PR #238 が**同じ ADR を別々の
ファイル名で二重デリバリ**した。worker は PR を開く前に「その issue にリンクされた
open PR が既に無いか」を見ない。両方マージされていたら ADR 0026 が2枚できていた。

### 3-E. インフラ・設定の故障が needs-human の枠を汚す

- herdr の ConnectionRefused が issue ごとに繰り返し escalation(23件)。retryable な
  インフラ故障が「人間の TODO リスト」(needs-human フィルタ)を占拠する。
- config 編集中の一瞬、run が pin していた profile が消え escalation(22:29)。

## 4. 改善設計(優先度順)

### P1: セッション健全性 — 「開くか」でなく「会話できるか」を resume の条件にする

**設計**:
1. resume 判定の拡張: pane 行の session を resume する前に、transcript サイズが
   閾値(例: プロファイル毎、既定 5MB)を超えていたら resume せず fresh spawn +
   full re-injection に落とす(プロンプトは自己完結しており文脈再構築は不要)。
2. 事後検知: 同一 turn_id で nudge が尽きて agent_quiet に落ちた回数を pane 行に
   数え、同一セッションで2回目の agent_quiet 落ちが起きたら
   `agent_session.cleared`(reason: `quiet_loop`)でセッションを破棄し fresh spawn。
   3回目で初めて needs-human。
   あわせて quiet 判定の前に mux の `AgentState` で **agent プロセスの在否**を見る:
   agent 不在(pane が素のシェル)なら nudge せず即 PaneDied 扱いにする —
   シェルへ nudge 文を打ち込む事故と、無意味な nudge 待ち(約3分×2)を消す。
3. 診断の同梱: awaiting_human(agent_quiet) の escalation コメント/イベントに
   **pane 末尾 N 行を添付**する。読むのは診断のためで、成否裁定には使わない —
   ADR 0026 の「read するが裁定しない」と同じ立て付け。overview の
   「画面を読んで成否判定しない」原則は破らない。

**受け入れ基準**: 400 恒久ループを fixture 化した統合テストで、人手なしに
fresh spawn へ復帰すること。agent_quiet の escalation に pane tail が含まれること。

**効果**: 今回最大の時間損失(13時間)がクラスごと消える。異種モデル(ADR 0023)を
増やすほど小さい context window のプロファイルが増える — 前提整備として必須。

### P2: エスカレーションの冪等化(read-after-write + comment dedup)

**設計**: `escalate_pr` / `escalate_budget_exhausted` / ci_fixer 同型箇所で、
1. `add_pr_label` の結果を捨てない。失敗したら次 sweep に委ねる(コメントも出さない)。
2. コメント投稿前に、現 head/同一 reason の既存 escalation コメントがあれば skip
   (merge_tail の `head_already_armed` マーカーと同じ、コメントをマーカーとして
   使う既存イディオムで実装できる)。

**受け入れ基準**: FakeForge で label 書き込みを1回失敗させても、escalation コメントが
高々1件であること。

### P3: blocking finding の機械照合(anchor verification)

**設計**: plan/impl レビューの finding に anchor(`path` + `line` 範囲 + **現物引用**)を
必須化し、meguri 側で defer/escalate の前に機械照合する:
引用文字列が対象 head の該当ファイルに存在しなければ、その finding は
`stale`(照合失敗)として棄却し、レビューへ「anchor 照合に失敗した。現 head を
読み直して再レビューせよ」と1回だけ差し戻す。ADR 0022 の findings 台帳に
`anchor_verified` を持たせ、stale 率を統計に出す(ADR 0026 の CATCH 品質指標)。

あわせて **reviewer ターンは fresh session を既定**にする(resume は fixer 系のみ)。
旧 head の記憶が現物より強くなる 3-B の構造原因を絶つ。レビューは毎回自己完結の
プロンプト(diff 同梱)なので resume の文脈価値がもともと薄い。

**受け入れ基準**: 存在しない引用を持つ blocking finding が needs-human に到達しない
こと(差し戻し1回→クリーンなら通過)。#231 の実ケースを fixture 化。

### P4: impl 側にも bounded fixer(guard blocking の自動 first-response)

**設計**: guard(impl) が blocking/findings を出したとき、即 needs-human ではなく
spec_fixer と対称の **impl_fixer 1〜2 round** を先に回す(findings をプロンプトに
同梱 → 修正 push → guard が新 head を再レビュー)。budget を使い切って still red
なら従来どおり needs-human。ADR 0025(guard は tripwire)とは矛盾しない —
止める判断は guard のまま、止まった後の一次対応を自動化するだけ。
ADR 0024 の注意(外部 reviewer findings は injection 面)は spec_fixer と同じ
sanitize 経路を通す。

**受け入れ基準**: guard blocking → fixer 修正 → guard 緑 → auto-merge が人手ゼロで
通る統合テスト。#238(定義2点の追記)がまさにこの形だった。

**効果**: 今回の needs-human 5件中 3件(#238、#239×2)がこの経路で人手不要になった
可能性が高い(#239 の2件も「指摘どおり直す」だけの修正だった)。

### P5: issue↔PR の重複デリバリ検出

**設計**: worker/spec-worker が PR を開く直前に、対象 issue にリンクされた open PR
(クロスリファレンス)を確認する。meguri ブランチ以外の PR が既にあれば PR を開かず
needs-human(「レール外の既存 PR と衝突。adopt するか閉じるか人間が決める」)。
同時に、planner が spec PR を開くときも同様に確認する。

**受け入れ基準**: FakeForge に既存リンク PR がある状態で worker が PR を開かない。

### P6.5: sweep の沈黙故障を可観測にする(追記: 2026-07-21 の実事故)

**実事故**: #227 で入った bulk-observe GraphQL 文字列の閉じ括弧が1個過剰で、
新バイナリ稼働以降 **merge-tail sweep が毎 poll 失敗**し、全プロジェクトで
arm / orchestrator merge / BEHIND 解消が停止していた(修正: #242)。
症状は `watch.log` の `WARN merge-tail sweep failed` のみ — issue にも通知にも
現れず、「PR が緑なのにマージされない」を人間が不審に思って掘るまで見えなかった。
FakeForge はこの文字列を実行しないため CI でも検出不能だった。

**設計**:
1. scheduler の各 sweep(merge-tail / handoff / reaper / …)の連続失敗回数を数え、
   閾値(例: 10 回 ≒ 5分)を超えたら `sweep.degraded` イベント + 通知(notify sink)
   に昇格する。ログ WARN 止まりにしない。
2. 実 forge に投げる固定クエリ文字列は const 化し、parse-level の検査
   (括弧バランス等)を単体テストに置く(#242 で merge-tail 分は実施済み。
   他の GraphQL 文字列にも同型のテストを足す)。
3. `meguri doctor` に「直近1時間の sweep 失敗率」を出す(DB の events だけで出せる)。

**受け入れ基準**: sweep を強制失敗させる fixture で、K 回連続失敗後に通知イベントが
1回だけ(冪等)出ること。

### P6: インフラ故障の escalation を needs-human と分ける

**設計**: forge/mux のコマンド失敗(ConnectionRefused 等)由来の escalation は
issue の needs-human ラベルに落とさず、`infra` 種別のイベント + 集約通知
(同一 reason は backoff 付きで1本化)にする。needs-human は「判断が要る」専用に保つ。

**受け入れ基準**: mux 停止中の sweep で issue に needs-human が付かないこと。

## 5. 計測 — 改善が効いたかを ADR 0026 の枠で問う

- **human roundtrips / merged PR**(escalation.raised→ラベル除去 のサイクル数)を
  `meguri stats review` に追加する。今回の実測: #234=2, #236=1, #223(spec)=3+。
  目標は「判断が要る指摘のみ = おおむね 1 以下」。
- needs-human の**滞留時間**(raise→clear)を出す。人間律速の可視化。
- P3 の stale 率、P1 の session rotate 回数も同じ statsに載せる。

## 6. 実施順序

P2(小・独立)→ P1(効果最大)→ P3(P2 とだけ干渉)→ P4(P3 の anchor 照合を前提に
すると安全)→ P5・P6(独立・小)。P1〜P4 は ADR 化してから実装(0022〜0026 と
整合を取る箇所が多い)。P5・P6 は issue 直行で足りる。

## 付録: 今回のランで人間ゲートが実際にやったこと

1. #231: stale finding を head と照合して棄却、spec-ready へ昇格(P3 があれば不要)
2. #238: guard 指摘2点を ADR に反映して push(P4 があれば不要)
3. #239: guard 指摘を検証し2回修正 push(P4 があれば1回は削れた)
4. #237: 重複 PR を close(P5 があれば発生しない)
5. #222: 死にセッションの transcript 退避 + run stop で fresh spawn を誘発
   (P1 があれば不要)
