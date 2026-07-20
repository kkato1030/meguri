# ADR 0026: schedules を repo-eligible にする — claim 時 pin ではなく default branch からの「発見」読み取りで二層化する

- Status: accepted
- Date: 2026-07-20
- Issue: #222

## コンテキスト

ADR 0011（二層 config）は「プロジェクト内在の設定は repo ルート `meguri.toml` に置ける」と定めた。
ただし `schedules`（#146）だけは例外として host 側に据え置いた。理由は表に「反映には write 権限が
要り境界内だが、**常設の実行トリガを repo に置く段差**があるため初期は host 側」と書かれ、判断は
issue #146 側へ預けられていた。

その #146 側の結論を出すのが本 ADR（ADR 0012 の移行スライス2 / #222）である。スライス2 は
`scheduler_fire` を Schedule Kind の reconciler に載せ替えると同時に、この積み残した二層化を取り込む。

「段差」を作っていた前提は、その後の ADR で既に解けている:

- **enqueue-only（ADR 0009）**。schedule が発火しても、やることは issue / task を1件作るだけである。
  pane も run record も作らない。人間が issue を1件立てるのと同じで、新しい権限が repo 側に宿るわけ
  ではない。
- **default branch からの発見読み取り（ADR 0015）**。「run に紐付かず・完了契約に効かない読み取りは
  マージ済みの default branch から読む」という出所ルールが既にある。schedule の `body_file` は
  もうこの経路で読んでいる。
- **境界原理（ADR 0011）**。「その repo 自身の run にしか影響しない=プロジェクト内在の事実」は
  repo 可。schedule はまさに「この repo をどの周期で起票するか」という内在の事実で、他 repo も
  host token も名指ししない。

つまり schedule は境界原理上とっくに repo 可であり、唯一残っていた「常設トリガの段差」も
enqueue-only + default branch 反映 + 人間マージゲートで解消できる。

## 決定

**schedule を repo-eligible にする。ただし完了契約の値（`check_command` 等）と違い、claim 時 pin では
なく、sweep 時に default branch の `meguri.toml` から読む「発見（discovery）読み取り」にする。**

### なぜ claim 時 pin ではなく default branch なのか

ADR 0011 の pin 機構は「run claim 時に worktree から一度読んで run に固定する」ものである。schedule の
発火は **run が存在しない sweep 時点**で起きるので、pin する run がそもそも無い。したがって pin 機構は
適用できない。

ここで ADR 0015 の分類表がそのまま効く。schedule 定義の読み取りは:

- run に紐付かない（sweep は run 発見の前段で回る）。
- 完了契約（`check_command` / clean tree / commits-ahead）に一切効かない。

ゆえに ADR 0015 の「助言 / 発見」行に属し、**default branch から読む**のが正しい。常設トリガの
宣言セマンティクスは「マージ済みの内容が効く」であるべきで、working tree の checkout 状態に左右されて
はならない。反映経路はそのブランチへの commit = write 権限 = README の信頼モデルの内側で、diff に現れ
人間マージゲート / branch protection のレビュー対象になる（監査可能）。

### 有効な schedule 集合の作り方

sweep 時の有効 schedule 集合 = host `[[projects.schedules]]` ∪ repo（default branch の
`meguri.toml`）の schedules。

- **統合は `name` キーの和**。
- **名前衝突は host が勝つ**（ADR 0011「host が最後に勝つ」）。repo 側の同名定義は落とし、
  `schedule.shadowed` を emit + warn する。**黙って無視しない**（routing / repo config と同じ
  「静かなフォールバックをしない」原則）。
- repo `meguri.toml` の parse / 検証に失敗したら、repo 由来の schedule は**「無いもの扱い」**に
  フォールバックし（warn + `repo_config.invalid` emit）、host schedule はそのまま発火する
  （ADR 0011「壊れた設定でプロセスを殺さない」）。個々の schedule 単位の検証エラー（cron 不正 /
  body と body_file の排他違反 / local × plan）は、その1件だけ落として残りは活かす。

### fetch 失敗時は repo schedule 層を abstain する（fail-closed）

発見読み取りは run に紐付かないので、run flow が `origin/<default_branch>` を保つ前提が届かない。managed
clone では ref が古いまま新 schedule を見落とし、あるいは既に削除された schedule を撃ち続けうる。そこで
schedule の解決は read の前に `origin/<default_branch>` を fetch する。**fetch に失敗した tick は、repo
schedule 層を撃たない・読まない・seed しない（abstain）**。host schedule はこの ref に依存しないので撃ち続け、
sweep 全体も止めない。次 tick で再 fetch して追いつく。

この選択は追加・削除の両方向を踏まえたものである。best-effort で stale ref にフォールバックすると、削除済み
schedule を撃ち続けてしまう（誤起票）。逆に abstain 下では、新 schedule の初回発見が遅れた窓は no-backfill で
失われるが、これは「観測前の窓は撃たない」という既存契約どおりの正しい挙動で、誤起票よりはるかに軽い。
`fetch_branch_tip` の fail-hard（1件の不可逆起票を gate する用途）に対し、こちらは sweep 全体（host 分含む）を
巻き込まないよう「repo 層だけ fail-closed」に留める。remote が無い repo は staleness が起きないので fetch を
要さず local `<default>` を authoritative に読む。

### 発火状態の境界は不変（ADR 0012 決定2）

最終発火時刻は今までどおり sqlite `schedule_state` に `(project_id, name)` で置く。schedule を
host ↔ repo で移しても **name が同じなら state を引き継ぐ**ので、取りこぼしも再バックフィルも起きない。
name を変えれば旧 state は取り残され、新 name は seed される（既存の「新規 schedule はバックフィル
しない」挙動そのまま）。この境界は本 ADR で変えない。

### 配信契約は at-least-once（取りこぼさない）

発火は「issue/task を作る（enqueue）→ `schedule_state` の窓を前進させる」の2手で、その間で kill されると
item は作られたが窓が古いまま残り、次 tick が同じ窓を**再発火（重複）**しうる。ここを exactly-once に
するには、非冪等な forge 作成に対し「発火前に既存 item を検索」か「窓を先に前進させてから enqueue」が
要る。後者は enqueue 前 crash で**取りこぼし**（at-most-once）に反転する。scheduler では「取りこぼさない」
方が重要なので、**enqueue → record の順を保ち、契約を at-least-once と定める**。重複は enqueue-only
（ADR 0009）ゆえ「余分な issue/task が1件」で人間に可視である。

**overlap guard はこの crash 境界を抑えない**点に注意する。guard は `schedule_state` の `last_key` の
open/closed を見るが、enqueue→record の crash では作った item の key が `record_schedule_fire` 前で保存されて
おらず、次 tick の guard は古い `last_key` しか見られない。したがって guard が抑えるのは「消化が遅く直近 item が
open のまま次の cron 窓が来た」通常の重なりだけで、crash 由来の重複は防げない。crash 境界の重複を抑える要素は
enqueue→record 窓の狭さだけであり、それでも enqueue-only ゆえ低害・可視というのが正直な線である。
「二重発火なし」を厳密保証とはしない — この線引きを本 ADR に残す。

## 帰結

- 同じ repo をどのホストで回しても、起票周期が一致し、schedule 定義が repo と一緒にバージョン
  管理される。
- opt-in。`meguri.toml` に schedules を書かない既存プロジェクトの挙動は完全に不変（host 側の
  `[[projects.schedules]]` だけが効く）。
- local mode でも動く。remote が無ければ local の default branch から読む（ADR 0015）。
- **schedule discovery は read の前に `origin/<default_branch>` を fetch し、失敗時は repo 層を abstain する**
  （上節）。sweep / doctor / `meguri schedules` は同じ effective-set resolver を通り、表示と実発火が同一集合を
  見る。
- `meguri doctor` / `meguri schedules` は default branch 上の repo schedules も含む有効集合を表示し、host/repo の
  shadow を報告する。host schedules が空でも repo-only プロジェクトを見落とさない（doctor が人間向けの検証面
  という ADR 0015 の役割分担どおり）。
- `RepoConfig`（per-run claim 時に worktree から読む型）にも `schedules` フィールドを持たせるが、**中身を
  検証しない寛容なフィールド**（未検証の raw 値）に留める。`deny_unknown_fields` の下で `[[schedules]]` の
  存在を許容しつつ、schedule 1行の型エラーが `check_command` 等の完了契約 pin を巻き添えにしないためである。
  schedule の型付き読み取りは sweep / resolver が default branch から別型で行う。
- **rollback には順序が要る**: default branch の `meguri.toml` に `[[schedules]]` が残ったまま #222 前の
  コードへ revert すると、旧 `RepoConfig`（`schedules` を知らない）が `deny_unknown_fields` でファイル全体を
  弾き、run の pin が host-only に落ちて `check_command` 等を失う。**先に default branch から `[[schedules]]` を
  除去**してからコードを revert する。後方互換 parse では塞げず順序で担保する（詳細は spec の migration）。

## 却下した代替案

- **schedule を claim 時 pin する**: 発火時点に紐付く run が無いので pin 先が無い。適用不能。
- **worktree（default branch でなく）から読む**: checkout 状態や未 commit 編集で宣言と実挙動が
  ズレ、managed bare clone 化で working tree が消えると壊れる（ADR 0015 が正した失敗モード）。
- **repo が host より優先**: 運用者がローカルで矯正できなくなる。host が最後に勝つ（ADR 0011）。
- **host 側据え置きの現状維持**: 同じ repo を別ホストで回すと schedule を手で複製することになり、
  repo とバージョン管理されない歪みが残る（ADR 0011 コンテキストが挙げた当の動機）。
