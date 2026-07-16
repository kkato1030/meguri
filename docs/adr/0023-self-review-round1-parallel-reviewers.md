# ADR 0023: self-review の round 1 を並列 reviewer 化する — 発散はヘテロ、収束はホモ、merge は機械的 union

- Status: proposed
- Date: 2026-07-16
- Issue: #214(親: #211、前提: #212 / ADR 0022)
- 関連: ADR 0006(AI レビューの内部ループ化)・ADR 0008(多レンズ review)・ADR 0011(combined も self-review)・ADR 0012(escalation 集約・2層モデル)・ADR 0020(効き目は統率面イベントで測る・merge は無差別 union)・ADR 0022(findings 台帳・挙動 escalation)

## Context

self-review の cap 落ちには故障モードが2つある。ADR 0022(#212)は1つ目
「毎ラウンド新規指摘が湧く(round 2+ のフル再レビュー)」を、round 2+ を
「解消確認+新規のみ」に絞って潰した。残るもう1つが **round 1 の recall 不足**である。

単一 reviewer の round 1 は、その1本のモデル・その1回の attention が見落とした指摘を
拾えない。見落としは round 2 以降に「新規指摘」として遅れて湧き、台帳の open を増やし、
cap を消費する。round 2+ の意味論を締めても、round 1 の網が粗ければ後半に漏れが押し寄せる。

round 1 の recall を**構造的に**上げたい。ただし単純な多重化は新しい害を生む:
並列 N 本にすれば幻覚由来の `needs_human` も N 倍になり、モデルを混ぜれば較正差
(厳しさの違い)が「片方だけが出す指摘」として cap 消費に化ける。

## Decision

**round 1 だけを並列 reviewer 化する。round 2+ は ADR 0022 の単独 anchor reviewer のまま。**
設定は `[[review.reviewers]]`(spec §config)。未指定なら現行の単一 reviewer で挙動不変。

### 1. fan-out / merge に orchestrator agent を置かない

- meguri が**決定的に** N 本 fan-out する。各 reviewer は独立の review ファイル
  (`self-review-<name>.json`)に書く。
- merge は Rust 側の**機械的 union**。findings は加法的(recall を上げるのが目的)なので、
  どちらが正しいかの裁定はしない。round 1 は台帳が空なので、union = 全 reviewer の findings を
  決定的順序で連結し、それぞれ新規 open エントリにするだけ。
- 重複・矛盾は**時間方向に直列化して**解消する:fix turn の作者(waive 権を持つ)と、
  round 2 の単独 reviewer が捌く。実行時に信頼度で取捨しない(ADR 0020 の union 据え置きと同型)。

裁定用の orchestrator agent を置けば、それ自身が新たな較正点・幻覚点・帰属不能点になる。
機械的 union はその全部を回避する。

### 2. `needs_human` だけは OR にしない — anchor 確認 turn を1つ挟む

並列 N 本のどれかが `needs_human` verdict を返しても、**即 escalate しない**。
anchor モデル(self-reviewer profile)による確認 turn を1つ挟む:

- anchor が「確かに人間が要る」と確認 → escalate(ADR 0012/0022 の needs_human 経路)。
- anchor が否定 → needs_human を取り下げ、anchor 自身の verdict/findings を union に畳んで続行。

複数本が同時に flag しても anchor 確認 turn は**1つ**(全 flag 理由をまとめて1回判断)。
稀な経路のみに bounded な確認を差すことで、幻覚 escalate の N 倍化を防ぐ。

### 3. 発散はヘテロ、収束はホモ

- **単一モデル構成 → lens 分割。** 同一 prompt × N はモデル温度を制御できず attention が
  相関して無意味。prompt(lens)を割って attention の非相関を作る。
- **複数モデル構成 → モデル単位分割**(各モデルに全 lens)。lens × model の行列は過剰で、
  どの指摘がどの軸由来かの帰属も不能になる。
- **round 2+ の収束 reviewer と decision 裁定は anchor モデル(self-reviewer profile)に固定する。**
  発散(round 1)はモデルを混ぜて recall を稼ぐが、収束は1本に絞る。混ぜたモデル間の較正差
  (厳しさの違い)が、round 2+ で「片方基準の指摘」として cap 消費に再発するのを防ぐ。

### 4. reviewer あたりの findings 上限

各 round 1 reviewer の prompt に「1本あたり最大 K 件」の上限を入れる。N 本 × 無制限だと
union が肥大し、fix prompt(全 open finding を列挙)が膨らんで author の attention を薄める。
上限は recall と fix 集中のトレードオフを取る安全弁。

## Consequences

- **round 1 の recall が1本の見落としに依存しなくなる。** 後半に漏れが押し寄せる構造が減り、
  cap 消費が下がる見込み。効果は #213 の profile 別 unique 貢献率で観測する(ADR 0020 の
  段階導入どおり、reviewer 属性付きイベントが入って初めて出せる)。
- **未指定なら byte-for-byte 挙動不変。** `[[review.reviewers]]` が空なら現行 `review_turn` を
  そのまま通る。並列経路は非空設定でのみ分岐する(spec の受け入れ観点)。
- **幻覚 escalate を N 倍にしない。** needs_human は OR せず anchor 確認を1つ挟む。稀経路
  だけの bounded なコスト。
- **実装前提が広がる。** per-turn 完了コントラクト・スケジューラのスロット予約・doctor probe の
  非 claude 対応が必要(spec §実装前提整備)。これらは round 1 並列化の土台であって、
  単独では挙動を変えない。
- **信頼境界が新設される。** 外部モデルの findings body が author の fix prompt に入る
  prompt injection 面は ADR 0024 に分けて記録する。
- **round 2+ と ADR 0022 は無傷。** 台帳・挙動 escalation・cap→final-fix は不変。本 ADR は
  round 1 の finder 段だけを差し替える。
