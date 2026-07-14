# ADR 0012: 詰まったら昇格 — シグナル駆動のプロファイルエスカレーションと explore canary

- Status: accepted
- Date: 2026-07-14
- Issue: #66(routing 3/3)
- 関連: docs/adr/0003-role-based-agent-routing.md(役割→プロファイル基盤)、
  docs/adr/0011-routing-role-6-kinds-of-work-independent-of-loop-kind.md、
  docs/adr/0007-routing-freshness-and-outcome-drift.md(stats routing / drift)

## 文脈

ADR 0003 の役割ベース振り分けは「この種類の仕事はどのモデルに」を静的に決める。だが
issue の難しさは事前には分からない。難易度を推定してから割り当てる道もあるが、推定器は
当たらないうえ保守コストが高い。meguri は難易度を**推定せず**、実際に出たシグナルに
反応して適応したい。needs-plan エスカレーション(#22)がすでにこの形をとっている
——「worker が設計判断を要ると気づいたら planner に渡す」。同じ発想を「モデルの強さ」に
広げる。

## 決定

### 1. プロファイルエスカレーションはシグナル駆動(難易度推定なし)

安いモデルで始め、**詰まったシグナルが出たら1段強いプロファイルで spawn し直す**。
トリガーは検証失敗の 2 回目(`validate_turns` を使い切る前)。「安く始めて詰まったら
昇格」は、平均的な issue のコストを下げつつ、難しい issue にだけ高いモデルを使う。

### 2. エスカレーションチェーンは検出フォールバックチェーンとは別物

既存の `recommended_chain`(routing.rs)は**検出フォールバック**用で、末尾は必ず
`default` に落ちる——つまり「弱くなる向き」。エスカレーションは逆に「強くなる向き」。
両者は目的が違うので別の表 `escalation_chain(role)` として持つ。既定は
`worker` / `fixer` = `["claude-sonnet", "claude-opus"]`、`planner` や reviewer 系は
すでに最上位なのでチェーンなし(=昇格しない)。トップレベル `[escalation]`(下記 §6)で
役割ごとに上書きできる。

昇格の起点は**現在 pin されているプロファイルのチェーン内位置**で決める。チェーン内で
末尾でなければ次の entry へ進み、**チェーン外のプロファイルからは昇格しない**。これで
routing 1/3(ADR 0003)の契約と噛み合う: manual・検出フォールバックで pin される
`default` は昇格対象外(`default` はエスカレーションチェーンに書けない——強さが定義
できず、チェーン内判定を濁すため validate で禁止)。明示 `[routing.roles]` でチェーン外の
プロファイルを pin した run もそのまま(明示は勝つ)。明示 pin がチェーン内にある場合は
昇格するが、これは「検出失敗の黙ったすり替え」とは別物だ——`run.escalated` イベントと
プロンプト明記で loud であり、`[escalation] enabled = false` か役割チェーンの上書き
(空チェーン)で独立に止められる。昇格先の候補は存在 + CLI 検出を確認し、使えなければ
チェーンの**上位へ読み飛ばす**(auto 解決が候補を読み飛ばすのと同じ流儀、ただし向きは
強い方のみ)。使える entry がなければ昇格せず、既存の needs-human バックストップに任せる。
config で上書きしたチェーンは起動時 validate で検査する。

### 3. モデルが変わる昇格は resume できない——新規セッション + 経緯の引き継ぎ

native session の resume(`claude --resume <id>`)はモデルに紐づく。プロファイルが
変わればセッションは再利用できない。よって昇格時は **live ペインを retire し、ペイン行の
`agent_session_id` を明示的にクリア**して、次ターンを新プロファイルで新規 spawn する。
失った文脈は、トリガープロンプトに**検証失敗の内容(コマンド・stdout/stderr・それまでの
経緯)を載せて**引き継ぐ。validate ループの fix プロンプトはすでに失敗内容を含むので、
そこに「モデルを上げて再挑戦している」旨を足すだけでよい。

### 4. `runs.agent_profile` は「いま起動しているプロファイル」、履歴は events + `routing_arm`

昇格は `runs.agent_profile` を新プロファイルで上書きする(pin の再解決は走らない)。
「どこから上がったか」は `run.escalated` イベント(from / to / level / reason)に残す。
ただし agent_profile を上書きすると、stats(`(loop_kind, agent_profile)` で集計)からは
「昇格して opus になった run」と「最初から opus の run」が区別できなくなる。そこで
`runs.routing_arm` 列を足し、各 run が **本線 / explore / escalated** のどれだったかを
1 語で持つ。これで stats routing(#65)が昇格率・explore 比較を本線と分けて出せる。

### 5. explore_ratio は opt-in の決定的 canary(既定 off)

「いまの割り当てが最善か」に将来も答え続けるには比較データが要る。だが explore は
**実 issue を代替プロファイルで実験台にする**ので、既定 `0.0`(off)。有効化すると、
issue 番号のハッシュで**決定的に**一部の run を選び、推奨チェーンの次候補プロファイルに
割り当てる。決定的なので再現・テストが可能で、同じ issue が毎回同じ腕に入る。explore で
割り当てた run は `routing_arm = "explore"` として stats で本線と分けて集計する。
`explore_ratio` は「routing の割り当て方」の一部で、on にする時点で routing を使う意思が
あるため `[routing]` 内に置く(既定 0.0 なので普段は書かれない)。

explore が canary する対象は **auto の推奨ピックだけ**である。明示 `[routing.roles]` で
指定された役割・manual mode・legacy では explore は no-op にする。ADR 0003 は「明示指定は
常に勝ち、その profile を使う」と約束しているので、`worker = "claude-sonnet"` と書いた人が
黙って次候補に差し替えられてはならない。auto で推奨が動いている役割だけが、その推奨と代替を
比べる母集団になる。

### 6. 設定は `[routing]` の存在契約を壊さない置き方にする

ADR 0003 は「`[routing]` が存在したら role routing 有効」を on/off 契約にした。もし
`escalation` を `[routing.escalation]` に入れると、TOML では
`[routing.escalation] enabled = false` と書くだけで親 `[routing]` が生まれ、`mode = auto` の
推奨解決が勝手に有効化される——「昇格だけ止めたい」設定が legacy を壊す。ADR 0007 が
`[routing.drift]` を避けてトップレベル `[drift]` にしたのと同じ判断で、独立に止めたい
`escalation` は**トップレベル `[escalation]`** に置く。

さらに、**エスカレーションも explore も routing が有効(`cfg.routing.is_some()`)なときだけ
働く**共通ゲートを置く。チェーンは推奨表由来の概念で、routing が off の場所で昇格が走るのは
筋が通らないからだ。この 2 つ(トップレベル配置 + 共通ゲート)で、`[escalation]` を書いても
role routing は勝手に有効化されず、legacy は無傷のまま `enabled = false` で昇格だけ止められる。

### 7. 有限で、既存のバックストップに戻る

- エスカレーションはチェーン末尾で止まる。そこで更に失敗したら**従来どおり
  `validate_turns` 超過 → needs-human**。無限昇格しない。
- explore は割り当てを変えるだけで、その上に昇格を積むことはしない。explore run も
  詰まれば昇格しうるが、**腕の記録(`routing_arm`)は explore を優先**する(explore >
  escalated > main)。explore は「代替と本線を比べる」母集団であり、そこから昇格した run を
  `escalated` に移すと比較の分母が崩れるため。昇格の事実は `run.escalated` イベントに残る。
- 旧挙動へ戻す条件は仕組みごとに別で、どれも `[routing]` の存在判定に触れない:legacy
  (`[routing]` なし)は共通ゲートで両方 inert。explore は `explore_ratio = 0`(既定)で
  inert。エスカレーションは既定 `enabled = true` で `worker`/`fixer` に既定チェーンがある
  ため、routing 有効環境では **トップレベル `[escalation] enabled = false` を明示**して
  はじめて昇格まで含めて routing 1/3 と一致する。routing 有効環境で両方止めれば**バイト単位で
  一致**する。

## 帰結

- 役割→プロファイル解決は「起動時に一度 pin」から「pin はするが**昇格で上書きされうる**」に
  変わる。resolve_run_profile は pin を読むだけなので、上書き後は自然に新プロファイルを返す。
- ペイン resume の前提「同じ役割の live ペインは常に adopt する」に例外が入る:
  昇格直後は adopt せず新規 spawn する(session_id クリアで表現)。
- `runs` に列が 1 つ増える(`routing_arm`)。既存 run は NULL = 本線として読む
  (agent_profile と同じ後方互換の足し方)。
- 新しい役割やループを足すときは、`routing_role_for_loop` に加えて、昇格させたいなら
  `escalation_chain` にもチェーンを定義する(なければ昇格しない、が安全な既定)。
