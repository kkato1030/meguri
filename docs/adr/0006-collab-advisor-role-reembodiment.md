# ADR 0006: 役割は死なせて再具現する — 実装中の助言レイヤ(collab-advisor)、相談は完了契約の外

- Status: accepted
- Date: 2026-07-12
- Issue: #111
- 関連: routing #64 / ADR 0003(その次段)、issue-lane 寿命 #92 / ADR 0004(第 3 lane の再訪)、impl-reviewer #84 #108 / ADR 0004(補完関係)

## 文脈

meguri の自律運用でいちばん漏れるのは「実装中のドリフト」だ。planner が良い spec を書いても、worker がそれを実装する頃には planner の run は spec PR を開いた時点で終わっており、pane は reaper が畳んでいる(#92 で寿命を issue 番号に束ねたが、束ねているのは同一 issue の**連なり**であって、フェーズをまたいで planner を生かし続けてはいない)。worker は spec というテキストだけを頼りに独りで進み、ズレても気づけるのは PR が出て impl-reviewer(#84/#108)が読む**後** — そこまでに commit は積み上がっている。

routing(#64)は「どの役割をどのモデルに振るか」を決めた。次に効くのは、**振り分けた役割モデル同士を実行中に喋らせること**だ。運用実感としては「完成したプランを実装モデルに自走させ、その間、実装中のモデルが元のプランを書いたモデルに『要件を満たしているか/ブレていないか』を適宜相談できる」。transport には [agmsg](https://github.com/fujibee/agmsg)(トランスポートは素朴に、プロトコルは各 agent のプロンプトに委ねる設計)を使う。

## 決定

**worker 実行中に、plan 作者の役割(planner)を助言者(advisor)として同じ issue に生かし、worker が agmsg 越しに「ズレてないか/要件を満たしているか」を相談できる助言レイヤを、オプトインで足す。** 三つの原則を固定する。

### 1. 役割は死なせて再具現する(session の復活ではない)

「元々のプランを作った Fable 5」はその時点でもう生きていない。planner run は終わり、pane は畳まれている。だから advisor は**死んだ個体の復活ではなく役割の再具現**にする — 同じ issue に advisor lane を新規に spawn し、planner プロファイル(routing の `advisor_role` 解決結果)で起動し、merge 済み spec で seed する。モデル・役割・spec が同じなら、それは実質「plan を作った Fable」だ。planner session をフェーズ跨ぎで延命する案は退けた:延命は meguri の寿命モデル(run は ephemeral、文脈は pane 行に置く)に逆行し、常駐コスト(サブスク枠)を垂れ流す。

これは **ADR 0004 の「lane の一般化(3 つ以上)はしない。必要になったときに再訪する」の再訪**だ。author / review の 2 lane に **advisor lane(第 3 の lane)**を意図的に足す。ただし advisor lane は他の 2 つと寿命規律が違う(下記 3)ので、ADR 0004 の「pane 回収は issue の状態だけ見ればよい」を崩さない範囲に閉じ込める。

### 2. 相談は助言であって完了条件ではない — meguri は agmsg を読まない・待たない・検証しない

完了契約は従来どおり `.meguri/result.json` + 独立検証(clean tree / commits ahead / check pass)だけで決まる。meguri は agmsg の SQLite floor を**一切パースしない**。相談の有無・内容・成否は run の成否に影響しない。agmsg を実行するのは agent 自身であり、meguri が agmsg を exec するのは起動時の存在検出(`agmsg --version`)ただ一度に限る。これは meguri の「durable signals only」不変条件(ADR 0004: forge 上の marker が Authority)の延長で、側路の会話をオーケストレーションの判断材料にしない。

帰結として、run 時の advisor spawn は **best-effort** にする。advisor pane の spawn が失敗しても worker run は止めない(助言レイヤの不在は完了契約を壊さない)。**起動時**の agmsg 未検出だけが致命(下記)。

### 3. advisor は ephemeral — worker run 終了で必ず reap、keep_pane に依存しない

advisor は worker の execute 開始で spawn し、worker run の終了(成功 / needs-plan / decompose / 中断 / 失敗のすべて)で reap する。author / review lane が `keep_pane = "until-issue-closed"` で issue close まで残るのとは**別規律**で、advisor は常駐させない。これは reaper(issue の close を見る)の仕事ではなく、run 終端での明示 release だ。reaper には安全網として advisor lane を掃く行を足すが、正常経路では run 終端で先に消えている。

## 帰結

- 実装 → 相談 → 実装が一本のセッション内で回り、ドリフトが「早く・安く」摘まれる。impl-reviewer(PR 後・非同期・徹底)とは**畳まず補完関係**:早く安くドリフトを摘む層 / 後で徹底的に欠陥を摘む層。
- 助言レイヤは meguri の観測面に一切現れない。agmsg が落ちていようが会話がゼロだろうが、run の成否は result.json + git 検証だけで決まる。オーケストレーションのデバッグ面を増やさない。
- 第 3 lane を足したが、ADR 0004 の「回収は issue の状態を見る」規律は保たれる — advisor は run 終端で明示 reap され、reaper 掃引は安全網に留まる。
- routing に新しい役割は足さない。advisor は既存の planner 役割のプロファイル解決を借りる(`advisor_role` 既定 `"planner"`)ので、routing 表は無変更で「plan を作ったモデル」がそのまま助言に立つ。
- v1 の対象は worker / spec-worker のみ。他役割への展開、多者 team、monitor/turn の delivery mode 調整、meguri による agmsg history からの stall 検知は将来。
