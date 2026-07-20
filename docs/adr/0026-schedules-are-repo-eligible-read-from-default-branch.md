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
（ADR 0009）ゆえ「余分な issue/task が1件」で人間に可視、かつ overlap guard（既定）が直近 item の open 中は
次発火を抑えるので実務上は稀である。「二重発火なし」を厳密保証とはしない — この線引きを本 ADR に残す。

## 帰結

- 同じ repo をどのホストで回しても、起票周期が一致し、schedule 定義が repo と一緒にバージョン
  管理される。
- opt-in。`meguri.toml` に schedules を書かない既存プロジェクトの挙動は完全に不変（host 側の
  `[[projects.schedules]]` だけが効く）。
- local mode でも動く。remote が無ければ local の default branch から読む（ADR 0015）。
- **schedule discovery は read の前に `origin/<default_branch>` を best-effort fetch する**。発見読み取りは
  run に紐付かないので、run flow が ref を保つ前提（`read_file_at_default_branch` は fetch しない）が届かず、
  managed clone では ref が古いまま新 schedule を永遠に見落としうる。同じく run を持たず起票を gate する
  decompose materializer が fetch するのと同型。fetch 失敗時は直近 ref にフォールバックし次 tick で復帰する
  （at-least-once の「取りこぼさない」に沿う）。
- `meguri doctor` は default branch 上の repo schedules も lint し、host/repo の shadow を報告する
  （doctor が人間向けの検証面という ADR 0015 の役割分担どおり）。
- `RepoConfig`（per-run claim 時に worktree から読む型）にも `schedules` フィールドを持たせる。
  run flow はこれを**使わない**が、`deny_unknown_fields` の下で worktree parse が弾かないために
  フィールドの存在が要る。schedule は sweep が default branch から別途読む。

## 却下した代替案

- **schedule を claim 時 pin する**: 発火時点に紐付く run が無いので pin 先が無い。適用不能。
- **worktree（default branch でなく）から読む**: checkout 状態や未 commit 編集で宣言と実挙動が
  ズレ、managed bare clone 化で working tree が消えると壊れる（ADR 0015 が正した失敗モード）。
- **repo が host より優先**: 運用者がローカルで矯正できなくなる。host が最後に勝つ（ADR 0011）。
- **host 側据え置きの現状維持**: 同じ repo を別ホストで回すと schedule を手で複製することになり、
  repo とバージョン管理されない歪みが残る（ADR 0011 コンテキストが挙げた当の動機）。
