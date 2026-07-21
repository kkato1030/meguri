# ADR 0026: レビューの効き目は COST×CATCH で測る — 0020 に token コストと反事実 arm を足す

- Status: proposed
- Date: 2026-07-21
- Issue: #236(0020 続きスライス — レビュー効き目の COST×CATCH 計測)/ 親 #211
- 関連: ADR 0020(自己レビューは統率面イベントで測る・union は据え置き、#213/#211)、
  ADR 0017(不可視な面は統率面 durable 信号でのみ測る、#121)、ADR 0013(profile escalation と explore canary、#66)、
  ADR 0022(findings 台帳・severity 不採用、#212)、ADR 0023(round1 並列 reviewer、#214)、
  ADR 0025(guard は安全 tripwire、#228)、ADR 0006(自己レビューは内部ループ・forge を触らない)

## 文脈

「今のレビュー編成は品質担保に必要なのか、それとも過剰か」を**データで**問えない。

- レビューのコストは meguri が載せているのではない。meguri は1ターン渡すだけで、その中で
  claude code cli が自律ループを回し、**ツール呼び出しのたびに成長中の全文脈を再送する**。
  実測: pr-reviewer の1レビューが **45往復・ピーク文脈 132K token・処理 input ≒ 390万 token**
  (`Agent()` によるサブレビュアー fan-out 込み)。1発の巨大リクエストではなく、荷物を45回運び直す構造。
- ところが計測は片肺だ。ADR 0020/#213(`meguri stats review`)は cap 落ち率・round 分布・waive 率
  という **CATCH の一部**は測るが、**COST(token)を全く測っていない**。片方が無ければ効率は出せない。
- ADR 0023 は round1 並列 fan-out を「recall が上がる**はず**」で入れたが、**限界 recall を測る手段が無い**。
  N倍のコストを未検証の仮説に払う構造で、有効化(gpt+grok reviewer)した瞬間に顕在化する。
- ADR 0025 は guard を tripwire に退かせたが、**tripwire でもフルレビュー(390万 token)は同じだけ走る**。
  「めったに止めないもののために毎回フルコスト」= over-provisioning の疑いがあるが、検定できない。

ADR 0020 は「reviewer profile 別の unique 貢献率は台帳/並列イベントが揃って初めて出せる段階導入」と
明言し、そこを保留した。その保留を、**COST 軸**と**反事実の arm** を足して埋める。

## 決定

**レビューの効き目を「レイヤ別の COST(token)× CATCH(限界的に拾った本物)」で測る。効率 = 限界catch / token。**
実行時の挙動(union merge・完了契約・scheduler)は 0020 のまま一切変えない。measurement は派生ビューに閉じる。

### 1. 軸A COST — ターン完了時に transcript usage を集計する telemetry sidecar

ターンが `result.json` を書いて完了した時点で、そのターンの claude セッション区間
(`~/.claude/projects/<worktree>/*.jsonl`)の usage を集計し、1レコードとして落とす:

```
review_cost{ turn_id, run_id, project, loop_kind, role, reviewer_profile, routing_arm,
             input_tok, cache_read_tok, output_tok, round_trips }
```

- **境界原則**: これは meguri が唯一「claude の内部(transcript)を読む」点である。ただし読むのは
  **成否裁定には使わない telemetry 専用**。完了契約(result.json)・git 検証・`check_command` は従来通りで、
  「画面を読んで成否を判定しない」(overview.md, ADR 0006)の原則は無傷。ADR 0017 が引いた
  「不可視な面は統率面 durable 信号で測る」を、token コストという別種の durable 信号へ広げる。
- **backend 非依存**: 直 claude でも cliproxyapi 経由(gpt/grok)でも usage は同じ jsonl 形式に載る。
  だから proxy 側で吐かせる案(cliproxyapi 依存・直行プロファイルを測れない)ではなく sidecar を採る。
  cache_read も透過的に取れる(1レビュー内は逐次で効くが、並列共有で崩れる — 後述の帰結)。
- **これが 0020 の保留を解く**: 並列 reviewer は各々別 lane(`self-review#<index>`)・別 run で走る
  (ADR 0023)。sidecar を turn/lane 粒度で keyed すれば `reviewer_profile` が自然に付く。0020 が
  「`runs.agent_profile` は著者 profile だから reviewer 別は出せない」と言った境界を、lane 別 turn の
  コストレコードが越える。

### 2. 軸B CATCH — 台帳から raised / fixed / unique を導出する

CATCH は新スキーマをほぼ足さず、既存の findings 台帳(ADR 0022/0023: `id / kind(defect|decision) /
status(fixed|waived) / reviewer_profile`)から導出する:

- `raised` / `fixed`(実際に直された ≒ 本物) / `waived`(理由付きで棄却 ≒ ノイズ)
- **`unique`** = その reviewer だけが挙げ、かつ fixed(= 限界貢献の observational な代理)
- guard は台帳を持たないので、`Blocking` verdict で実際に auto-merge を止めた回数を **blocking_saves**(真の save)とする(ADR 0025 の三値 verdict をそのまま信号に使う)

**ground truth は段階導入**:
- **Phase 1**: `fixed vs waived`(+ waive 理由)を「本物か」の代理とする。完璧ではない(著者が
  reviewer を宥めるために非バグを直す/本物を waive する余地)が、人手ラベル無しで得られる最良の信号。
- **Phase 2**: 下流シグナル(マージ後 revert・マージ済み PR の CI 落ち・human reopen)を review の
  catch に紐付ける。0020 と同じく「イベントがある時だけ表示する」段階導入で、骨格を先に通す。

### 3. 導出メトリクス — `meguri stats review` の拡張

軸A×軸B を join し、`(project, loop_kind, reviewer_profile, routing_arm)` 別に読む
(既存 stats と同じ sqlite 直読み):

- reviewer_profile 別 **`unique_fixed / 1k token`** ← 「品質-caught / token」の本体。keep/drop 判断の主軸。
- レイヤ別(self-review / guard)のトークン総量と終端イベント分布(0020 の三値)を並置。
- **guard 効率 = `blocking_saves / guard 総トークン`** ← 「tripwire なのに高コスト」仮説を直接検定。
- 既存の cap-escalation 率(#213)に**コスト列を交差**させる。

### 4. 反事実 — observational を先行、canary は opt-in

`unique` は observational であり**選択バイアスが残る**(他 reviewer も挙げた finding は「冗長」に見えるが、
その reviewer 単独なら唯一の catch だったかもしれない)。これは因果ではない、と 0020/0017 と同じく正直に置く。

限界 recall を厳密に問う時だけ、**ADR 0013 の `explore_ratio` canary を再利用**する:
- control 群(reviewer 単独 / fan-out off / guard off)と treatment 群を hash で無作為割当し、
  cap-escalation 率・下流の escaped-defect シグナルを比較する。`runs.routing_arm` は既に arm を記録している。
- ただし canary は control 群に品質リスクを負わせる。だから**常時は回さず**、「fan-out を on にするか」
  「guard/impl を残すか」といった**編成変更の意思決定時にだけ opt-in で回す**期間限定の実験とする。

### 5. 実行時挙動は不変

finding merge は無差別 union のまま(0020 据え置き)。どの reviewer を採る/外すか・guard を軽量化するかは、
蓄積した stats を見て**人間がオフラインで決める**。measurement が実行時に重み付けや取捨をすることはしない。

## 帰結

- 「今のレビューは過剰か」に**数字で答える枠**ができる。特に (a) 並列 reviewer の `unique_fixed/token`、
  (b) guard の `blocking_saves/token` の二つが、ADR 0023/0025 で仮説のまま入った部分を事後検定する。
- **新しい境界を一点だけ引く**: COST 軸のために meguri が transcript の usage を読む。これは telemetry 専用で
  裁定に使わない、と明示する。この一点以外は 0020 の観測境界(統率面イベントの派生ビュー)を踏襲する。
- sidecar が backend 非依存なので、直 claude と cliproxyapi 経由(異種モデル)を**同じ土俵で**比較できる。
  加えて cache_read が可視化され、「並列 reviewer が1 codex 認証を共有 → 逐次キャッシュが崩壊 →
  毎往復フル再課金」という運用地雷(ADR 0023 が並列化時に検討しなかったコスト面)を数値で捕捉できる。
- observational な `unique` は交絡を残す(0020 と同じ正直さ)。canary を回した期間だけ因果に近づく。
  常時 canary はコスト側の過剰になるので、意思決定時 opt-in に留める。
- 段階導入: **sidecar(COST) → stats 拡張 → 下流シグナル(Phase2) → canary(opt-in)**。骨格を先に通し、
  信号は後から足せる形にする(ADR 0017/0020 の帰結と同型)。self-review/guard の不変条件は無傷。
- reviewer 別 attribution は「並列 lane 別 turn の COST レコード」に依存する。単独 reviewer(`reviewers=[]`)
  構成では reviewer 別の分解は出ず、レイヤ別(self-review 全体 / guard)までに留まる — この境界を明示しておく。
