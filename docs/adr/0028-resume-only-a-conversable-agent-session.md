# ADR 0028: resume の条件は「pane が開くか」でなく「会話できるか」

- Status: Accepted
- Date: 2026-07-21
- 関連: issue #245 / 設計書 `docs/design/needs-human-friction-and-delivery-speed.md` §3-A・§P1 /
  ADR 0004(pane・session の lifetime)/ ADR 0012(needs-human は判断専用)/
  ADR 0025・0026(read するが成否裁定はしない)

## 背景

meguri は agent の native session を `claude --resume <id>` で復元して turn をまたいで
文脈を引き継ぐ(ADR 0004)。resume するかどうかの判定は長らく「pane row に session id が
あるか」と「resume した pane が即死しないか」(`resumed_pane_survives`)の2点だけだった。

これが崩れたのが gpt プロファイル(context window が小さい)の pr-reviewer である。
session が context 100% に達すると、以後の入力はすべて `API Error: 400 input exceeds the
context window` になる。だが pane は開いたまま(即死しない)なので健常と誤認され、
nudge→agent_quiet→crash recovery→同一 session を resume→また 400、を13時間以上ループした
(#222、同日 #235 でも再発)。派生として、人間が復旧を試みて agent が exit した後の pane は
素の zsh に落ちるが、pane 生存だけを見る検出はそこへ nudge 文を打ち込み続けた。

「pane が開く」は「その session でまだ会話できる」を意味しない。この乖離が損失の中心だった。

## 決定

resume の健全性を、単発の生存確認から**多層のゲート**に変える。判断基準は一貫して
「その session はまだ会話できるか」であり、できないと分かった session は人手を待たずに
捨てて張り直す(ADR 0006 の原則をセッションにも適用する)。

1. **resume 前の transcript サイズゲート**。session を resume する前に transcript ファイルの
   バイト数を検査し、プロファイル毎の閾値(既定 5MiB)を超えていたら resume せず、
   session id を捨てて fresh spawn + full re-injection に落とす。プロンプトは自己完結して
   いるので文脈の再構築は要らない。context 100% は必ず巨大な transcript を伴うので、
   400 が起きる前に安く弾ける。

2. **quiet ループの有界化**。nudge を撃ち尽くして無応答(agent_quiet)に落ちた回数を pane row
   に数え、同一 lane で 2 回目に達したら session を破棄(`agent_session.cleared` /
   reason `quiet_loop`)して fresh spawn、3 回目で初めて needs-human にする。healthy な turn が
   1 回でも通ればカウンタは 0 に戻す。「無応答を無限に人間へ委ねる park」を、
   「一度だけ resume で試す→捨てて張り直す→それでも駄目なら人間」という有界の機械に置き換える。

3. **agent の在否を mux の一級の能力にする**。pane 生存とは別に「その pane に agent がいるか」を
   問う `agent_present` を mux に足す。`AgentState::Unknown` に相乗りさせない —— Unknown は
   tmux のヒューリスティックでは生きた agent でも普通に起きるので、在否の判定材料にすると
   誤検出する。agent 不在(素のシェル)の pane は、adopt せず・nudge せず、会話不能な session と
   同じ有界の機械に載せる。

4. **エスカレーションに診断を同梱するが、裁定には使わない**。agent_quiet で needs-human に上げる
   ときは pane 末尾 N 行(既定 25)をコメントとイベントに添える。これは人間が真因を掴むための
   read であって、成否の裁定には一切使わない —— overview の「画面を読んで成否判定しない」原則、
   ADR 0025・0026 の立て付けをそのまま踏襲する。

## 根拠

- 損失の中心(1件で13時間超)が session ローテーションのクラスごと消える。異種モデル(ADR 0023)を
  増やすほど context window の小さいプロファイルが増えるので、その前提整備として要る。
- サイズゲート(安く・事前に弾く)と quiet カウンタ(事後の取りこぼしを拾う)は目的が違うので
  併存させる。どちらか片方では、閾値直下で 400 になる session や、context 以外の理由で黙る
  session を取りこぼす。
- 在否を Unknown に相乗りさせない判断は、tmux の Idle/Unknown 判定を壊さないための構造的な選択で
  あり、後から効いてくるので ADR に残す。
- 診断同梱を「read するが裁定しない」と明記するのは、pane を読み始めた実装が成否判定に流用される
  退行を防ぐため。overview の原則との整合を将来のレビューが確認できるようにする。

## 影響

- turn の結果に「無応答で有界に諦めた」を表す終端 outcome が増える(park の無限待ちを置き換える)。
- pane registry に quiet カウンタ列が1つ増える(加算のみ・後方互換。詳細は spec の migration 節)。
- mux トレイトに `agent_present` が1つ増える(既定 `true` で既存 mux の挙動は不変)。
- agent プロファイルに transcript サイズ閾値の設定が1つ増える(既定 5MiB・省略可)。

具体的な touch 点・イベント名・テスト・migration/rollback は使い捨ての実装 spec
`docs/specs/issue-245.md` にある(実装が land したら spec は捨て、この ADR が残る)。
