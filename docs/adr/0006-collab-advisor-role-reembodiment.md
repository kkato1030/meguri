# ADR 0006: 役割は死なせて再具現する — 実装中の助言レイヤ(collab-advisor)、相談は完了契約の外

- Status: accepted
- Date: 2026-07-12
- Issue: #111
- 関連: routing #64 / ADR 0003(その次段)、issue-lane 寿命 #92 / ADR 0004(第 3 lane の再訪)、impl-reviewer #84 #108 / ADR 0004(補完関係)、collab 基盤 #121(横断制約の上位 issue)

## 文脈

meguri の自律運用でいちばん漏れるのは「実装中のドリフト」だ。planner が良い spec を書いても、実装が始まる頃には「spec を書いた本人」に外から相談できる相手はどこにも居ない。既定運用(`keep_pane = "until-issue-closed"`)では planner の session は死んでいない — author lane の pane として issue の寿命いっぱい生き、worker は**まさにその session を継いで**(resume して)実装に入る(ADR 0004:「同じ branch を編集する仕事は同一 pane・同一 session で文脈を継ぐ」、`src/engine/flow.rs` `ensure_pane` の設計)。つまり plan 作者は worker に**転生済み**で、実装に没入した当人に「お前はブレていないか」と独立の視点から問える相手が居ない。`keep_pane = "never"` なら session は run 終端で畳まれていて、そもそも相手が居ない。どちらの運用でも、worker は spec というテキストだけを頼りに独りで進み、ズレに気づけるのは PR が出て impl-reviewer(#84/#108)が読む**後** — そこまでに commit は積み上がっている。

routing(#64)は「どの役割をどのモデルに振るか」を決めた。次に効くのは、**振り分けた役割モデル同士を実行中に喋らせること**だ。運用実感としては「完成したプランを実装モデルに自走させ、その間、実装中のモデルが元のプランを書いたモデルに『要件を満たしているか/ブレていないか』を適宜相談できる」。transport には [agmsg](https://github.com/fujibee/agmsg)(トランスポートは素朴に、プロトコルは各 agent のプロンプトに委ねる設計)を使う。

## 決定

**worker 実行中に、plan 作者の役割(planner)を助言者(advisor)として同じ issue に生かし、worker が agmsg 越しに「ズレてないか/要件を満たしているか」を相談できる助言レイヤを、オプトインで足す。** 三つの原則を固定する。

### 1. 役割は再具現する(carryover の延命でも分岐でもない)

既定運用では「元々のプランを作った Fable」の session は author lane に生きている — が、worker がそれを継いで実装している最中だ。同一 session に advisor を兼ねさせれば自問自答になり、独立の視点が失われる(reviewer が author lane と別の review lane に session を分けて「視点を汚さない」のと同じ理屈)。`keep_pane = "never"` なら session 自体が畳まれている。だから advisor は **author lane carryover の延命・分岐ではなく役割の再具現**にする — 同じ issue に advisor lane を新規に spawn し、planner の役割プロファイル(その issue の planner run が pin した profile を最優先で継ぐ)で起動し、spec で seed する。モデル・役割・spec が同じなら、それは実質「plan を作った Fable」だ。author / review lane の寿命規律(ADR 0004)は無変更 — advisor は carryover を置き換えず、第 3 の lane として並走する。planner 専用 session をフェーズ跨ぎで別途常駐させる案は退けた:meguri の寿命モデル(run は ephemeral、文脈は pane 行に置く)に逆行し、常駐コスト(サブスク枠)を垂れ流す。

これは **ADR 0004 の「lane の一般化(3 つ以上)はしない。必要になったときに再訪する」の再訪**だ。author / review の 2 lane に **advisor lane(第 3 の lane)**を意図的に足す。ただし advisor lane は他の 2 つと寿命規律が違う(下記 3)ので、ADR 0004 の「pane 回収は issue の状態だけ見ればよい」を崩さない範囲に閉じ込める。

### 2. 相談は助言であって完了条件ではない — meguri は agmsg を読まない・待たない・検証しない

完了契約は従来どおり `.meguri/result.json` + 独立検証(clean tree / commits ahead / check pass)だけで決まる。meguri は agmsg の SQLite floor を**一切パースしない**。相談の有無・内容・成否は run の成否に影響しない。agmsg を実行するのは agent 自身であり、meguri が agmsg に触れるのは起動時の存在検出(skill 実体の version script)ただ一度に限る。これは meguri の「durable signals only」不変条件(ADR 0004: forge 上の marker が Authority)の延長で、側路の会話をオーケストレーションの判断材料にしない。

帰結として、run 時の advisor spawn は **best-effort** にする。advisor pane の spawn が失敗しても worker run は止めない(助言レイヤの不在は完了契約を壊さない)。**起動時**の agmsg 未検出だけが致命(下記)。

### 3. advisor は ephemeral — worker run 終了で必ず reap、keep_pane に依存しない

advisor は worker の execute 開始で spawn し、worker run の終了(成功 / needs-plan / decompose / 中断 / 失敗のすべて)で reap する。author / review lane が `keep_pane = "until-issue-closed"` で issue close まで残るのとは**別規律**で、advisor は常駐させない(この別規律は `keep_pane` の両値で同一 — 既定運用でも `"never"` でも、advisor は run 終端で消える)。これは reaper(issue の close を見る)の仕事ではなく、run 終端での明示 release だ。reaper の掃引は安全網に留まり、正常経路では run 終端で先に消えている。

ephemeral の系として、上位 issue #121(collab 基盤)の横断制約を三つ固定する:

- **read-only は配線で保証する。** advisor に書き込み可能な worktree を持たせない — repo のコピー自体を渡さない(cwd は git 登録の無い空ディレクトリ)。「コードを書くな」はプロンプトの願いではなく、書く場所が無いという配線の事実にする。
- **advisor もスロットを消費する。** advisor はサブスク枠で動く実 agent であり、会計外の常駐を作らない — `max_concurrent_runs` の予算を消費し、primary(worker run)の終了で必ず reap する。
- **再起動・resume では advisor を捨てて張り直す。** 生き残った advisor pane の adopt はしない。advisor の文脈は seed(spec)だけで再現できる設計だから、古い個体を信用するより捨てる方が安い。

## 帰結

- 実装 → 相談 → 実装が一本のセッション内で回り、ドリフトが「早く・安く」摘まれる。impl-reviewer(PR 後・非同期・徹底)とは**畳まず補完関係**:早く安くドリフトを摘む層 / 後で徹底的に欠陥を摘む層。
- 助言レイヤは meguri の観測面に一切現れない。agmsg が落ちていようが会話がゼロだろうが、run の成否は result.json + git 検証だけで決まる。オーケストレーションのデバッグ面を増やさない。
- 第 3 lane を足したが、ADR 0004 の「回収は issue の状態を見る」規律は保たれる — advisor は run 終端で明示 reap され、reaper 掃引は安全網に留まる。
- routing に新しい役割は足さない。advisor のプロファイルは、その issue の直近の planner run が pin した profile(`runs.agent_profile`)を最優先で継ぎ、無ければ planner 役割(`advisor_role` 既定 `"planner"`)の現在の解決結果に落ちる — routing 表は無変更のまま、可能な限り「plan を実際に作ったモデル」が助言に立つ。
- **編成 DSL は作らない(#121)。** config は `[collab]` の `mode` / `advisor_role` に留め、役割ペアの宣言・多者編成・graph 定義といった編成の言語化は、worker↔advisor に次ぐ 2 例目の編成パターンが現れるまで封印する。
- v1 の対象は worker / spec-worker のみ。他役割への展開、多者 team、monitor/turn の delivery mode 調整、meguri による agmsg history からの stall 検知は将来。
