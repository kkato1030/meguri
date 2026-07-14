# ADR 0012: 詰まったら昇格 — シグナル駆動のプロファイルエスカレーションと explore canary

- Status: proposed
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
すでに最上位なのでチェーンなし(=昇格しない)。`[routing.escalation]` で役割ごとに
上書きできる。

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

### 6. 有限で、既存のバックストップに戻る

- エスカレーションはチェーン末尾で止まる。そこで更に失敗したら**従来どおり
  `validate_turns` 超過 → needs-human**。無限昇格しない。
- explore は割り当てを変えるだけで、その上に昇格を積むことはしない。explore run も
  詰まれば昇格しうるが、**腕の記録(`routing_arm`)は explore を優先**する(explore >
  escalated > main)。explore は「代替と本線を比べる」母集団であり、そこから昇格した run を
  `escalated` に移すと比較の分母が崩れるため。昇格の事実は `run.escalated` イベントに残る。
- 旧挙動へ戻す条件は 2 つの仕組みで別々:explore は `explore_ratio = 0`(既定)で inert。
  一方エスカレーションは既定 `enabled = true` で `worker`/`fixer` に既定チェーンがあるため、
  `[routing.escalation]` を未設定にしただけでは止まらない——**`enabled = false` を明示**して
  はじめて routing 1/3 と一致する。両方を止めれば**バイト単位で一致**する。

## 帰結

- 役割→プロファイル解決は「起動時に一度 pin」から「pin はするが**昇格で上書きされうる**」に
  変わる。resolve_run_profile は pin を読むだけなので、上書き後は自然に新プロファイルを返す。
- ペイン resume の前提「同じ役割の live ペインは常に adopt する」に例外が入る:
  昇格直後は adopt せず新規 spawn する(session_id クリアで表現)。
- `runs` に列が 1 つ増える(`routing_arm`)。既存 run は NULL = 本線として読む
  (agent_profile と同じ後方互換の足し方)。
- 新しい役割やループを足すときは、`routing_role_for_loop` に加えて、昇格させたいなら
  `escalation_chain` にもチェーンを定義する(なければ昇格しない、が安全な既定)。
