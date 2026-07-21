# ADR 0028: セッション健全性 — resume は「開くか」でなく「会話できるか」で決める

- Status: Accepted
- Date: 2026-07-22
- 関連: issue #245 / 設計書 `docs/design/needs-human-friction-and-delivery-speed.md` §3-A・§P1 /
  ADR 0004(issue-lane pane session lifetime) / ADR 0026(read するが裁定しない)

## 背景

小さい context window のプロファイル(例: `gpt`)の reviewer セッションが context 100% に
達すると、以後の入力は全て `API Error: 400` になり無応答になる。現行の resume 判定は
「pane が即死しないか」(`resumed_pane_survives` の5秒プローブ)だけを見るため、
**開くが全メッセージ 400** のセッションを健常と誤認する。結果、nudge→agent_quiet→
crash recovery→同一セッション resume→400 を人手が入るまでループした(#222 で13時間超)。

派生症状として、人間が transcript を退避した後も pane 行の session id が残り、次 run が
同じ id を resume → agent が即終了 → pane が素の zsh に落ちる。以後の nudge は
シェルに打ち込まれる。**pane 生存 ≠ agent 生存** を現行の quiet 検出は区別できない。

## 決定

resume の条件を「pane が開くか」から「会話できる見込みがあるか」に変える。4点。

### 1. resume 前に transcript サイズを検査する

pane 行の session を resume する前に、その transcript ファイルのサイズを見る。
閾値(既定 5MiB、プロファイル毎に上書き可、`0` で無効)を超えていたら resume せず、
session id を破棄して fresh spawn + full re-injection に落とす。プロンプトは自己完結
なので文脈再構築は要らない。context 100% の代理指標として、恒久 400 に至る前に断つ。

### 2. agent_quiet は「無限 park」でなく「打ち切って回す」

nudge を撃ち尽くした quiet は、これまで `turn.awaiting_human` を出して**同じターンで
待ち続けていた**。これを打ち切って turn を終了させ、セッションを回す。同一セッション
(= issue × lane)での連続 quiet 回数を pane 行に数え、

- 1回目: セッションは残す(一過性かもしれない)。再試行は同じ session を resume。
- 2回目: `agent_session.cleared`(reason: `quiet_loop`)でセッションを破棄し、次で fresh spawn。
- 3回目: 初めて needs-human(自動復旧を諦め、人間へ)。

回数は完了ターン(`Completed`)で 0 にリセットする — 一度でも会話が成立したら健全。

「park して人間を待つ」旧動作は復旧不能ループの温床だったので廃止する。人間が能動的に
救いたい場合の経路は `Takeover`(quiet 判定より前に honor される)に一本化する。

### 3. agent プロセスの在否を見る

mux の `AgentState` に `Absent`(pane は生きているが agent プロセス不在 = 素のシェル)を
足す。quiet 判定の前に `Absent` を見たら、nudge せず即 `PaneDied` 扱いにする。
resume 直後の生存プローブ(`resumed_pane_survives`)も `Absent` を「生存せず」と判定する。
これでシェルへ nudge を打ち込む事故と、無意味な nudge 待ちが消える。resumed だった pane の
`PaneDied` は既存経路でセッションを破棄するので、素のシェル落ちループも断てる。

`Absent` は mux が持つ**プロセスの在否**であって画面スクレイプではない。tmux は
`pane_current_command` が既知のシェルかで、herdr は native な agent 状態で判定する。
overview の「画面を読んで成否を判定しない」原則は破らない(ADR 0026 と同じ立て付け)。

### 4. 診断に pane 末尾 N 行を同梱する

3回目の needs-human エスカレーション(および各 quiet イベント)に、pane 末尾 N 行を
添付する。読むのは診断のためだけで、成否裁定には使わない — ADR 0026 の
「read するが裁定しない」と同じ。真因(400 なのか、素のシェルなのか)が pane を開かずに
分かるようにする。

## 帰結

- 永続状態(pane 行の quiet 回数カラム)・config スキーマ(閾値)・`TurnOutcome` /
  `AgentState` の拡張・mux トレイトのプロセス在否検出が増える。移行と後退の詳細は
  実装 spec(`docs/specs/issue-245.md`)の migration & rollback に置く。
- 復旧不能セッションは高々3回のローテートで人間へ届く。13時間ループがクラスごと消える。
- 異種モデル(ADR 0023)を増やすほど小 context window のプロファイルが増える。その前提整備。
- 旧「park して待つ」動作に依存したテストは新動作に更新する。
