# ADR 0028: blocking finding は現物引用で機械照合し、reviewer ターンは fresh session を既定にする

- Status: proposed
- Date: 2026-07-22
- Issue: #247(親: #241 設計書「needs-human 摩擦」§3-B / §P3)
- 関連: ADR 0022(findings 台帳・挙動 escalation)、ADR 0023(round1 並列 reviewer)、
  ADR 0026(レビューの効き目 = COST × CATCH)、ADR 0008(plan/impl 対称レビュー)、
  ADR 0006(AI レビューは内部ループ)、ADR 0004(issue lane の pane/session lifetime)、
  ADR 0012(escalation 集約)、ADR 0025(guard は安全 tripwire)

## Context

self-review が「直っているものを直っていない」と言い、偽の不収束で人間を呼ぶ事故が起きた。

- 実ケース(#231、設計書 §3-B): reviewer が「head に存在しない文字列」への指摘を再主張し、
  fixer の budget が空転して needs-human に落ちた。人間ゲートは「その引用を head と照合したら
  存在しない → 棄却」で1秒で片付けた。**機械照合できる判定を人間にやらせていた**。
- 構造原因: reviewer セッションが resume されると、**旧 head の記憶が現物ファイルの読み直しに
  勝つ**。レビューは毎回、diff 同梱の自己完結プロンプトで動くので、resume が持ち込む会話文脈は
  もともと価値が薄く、害(古い記憶)のほうが大きい。

meguri の原則は「エージェントの画面を読んで成否を判定しない」だが、**finding の引用が対象 head に
実在するか**は画面読解ではなく機械照合であり、この原則を破らない(ADR 0026 が COST を「read する
が裁定に使わない」と切り分けたのと同型)。

## Decision

### 1. structured finding に現物引用(anchor)を必須化し、meguri が機械照合する

内部 self-review(ADR 0022 の findings 台帳)の `defect` finding に、既存の `path` + `line` に
加えて **現物引用 `quote`(対象ファイルに逐語で存在するはずの短い抜粋)** を必須化する。

- meguri は finding を台帳に畳み込む前に、`quote` が **現 head の該当ファイル(`path`)に逐語で
  存在するか**を照合する。存在しなければその finding は `stale`(照合失敗)とみなす。
- 照合は substring 一致で行う。`line` は人間と fixer のための位置ヒントであり、照合の一致条件には
  含めない(古い行番号で正しい引用を落とさない)。
- **`decision` 型 finding は照合対象外**(ADR 0022)。decision は「A か B かを決めて spec/impl に
  **追記**せよ」型で、既存コード文字列を指すとは限らないため、`quote` は任意、照合は skip する。

### 2. stale は1回だけ差し戻し、なお stale なら棄却する(needs-human に落とさない)

stale finding が1つでもあれば、その review ターンへ **1回だけ**「anchor 照合に失敗した。現 head を
読み直して再レビューせよ(該当 finding の一覧付き)」と差し戻す。

- 差し戻し後の再レビューで照合を通れば通常どおり台帳へ。
- 再レビューでもなお stale な finding は **台帳に入れず棄却**する(fixer を回さない)。verified な
  finding だけで phase を続ける。これで「存在しない引用による偽の不収束 → needs-human」経路が閉じる。
- 差し戻しは既存の corrective-turn 経路(tree 汚し・id 不整合の1回差し戻し)と同じ立て付けで、
  **anchor 照合失敗を追加の差し戻し理由**にするだけ。tree 汚しと anchor 失敗は独立に高々1回ずつ。

### 3. reviewer ターンは fresh session を既定にする(resume は fixer 系のみ)

reviewer ロール(`self-reviewer` / `pr-reviewer`)のターンは、pane 行に保存された
`agent_session_id` を **resume に使わない**。毎ターン素の spawn + フルプロンプト再注入で始める。

- author lane(worker/planner/spec-worker と、そこへ相乗りする fixer 系 = fixer/spec-fixer/
  ci-fixer)は従来どおり resume する — fix は直前の実装文脈が効くからだ。
- これで #231 の構造原因(旧 head の記憶が現物より強い)を根から絶つ。#231 の実インシデントは
  pr-reviewer(prose findings)側だったが、**fresh session 既定はロープ横断の session lifecycle
  変更**で、内部 self-review と外部 pr-reviewer の両方に効く。
- observability のための session id 保存自体は残す(`meguri ps` / 診断)。resume の**読み取り**を
  reviewer ロールで止めるだけで、ADR 0004 の lane = issue スコープの resumable context という
  枠は author/fixer 側で不変。

### 4. anchor 照合の結果を台帳と統計に出す(ADR 0026 の CATCH 品質)

- `LedgerEntry` に `anchor_verified: bool` を足す(照合を通った/免除された finding は true)。
- stale 率は `self_review.anchor_stale` イベント(棄却・差し戻しの件数)から `meguri stats review`
  が導出する。「イベント発火点 = stats のソース」という self-review 既存イディオムに合わせ、
  台帳フィールドを stats の母集団にしない。

## スコープ(この ADR が**やらないこと**)

- **pr-reviewer の prose findings の構造化・anchor 照合はやらない。** pr-reviewer は今も
  `review` 散文を PR body に畳む契約で、per-finding の構造 anchor を持たない。ここに anchor 照合を
  入れるには pr-reviewer の出力契約を構造化 finding へ作り替える別作業が要る。#247 では
  **pr-reviewer には fresh session だけ**を効かせ、構造 anchor 照合は内部 self-review 台帳に限る。
  pr-reviewer の finding 構造化は将来の follow-up。

## Consequences

- **機械照合できる判定が人間ゲートから外れる。** 「存在しない引用」による偽の不収束が
  needs-human に到達しなくなる(差し戻し1回 → クリーンなら通過、なお stale なら静かに棄却)。
- **reviewer コントラクトが広がる。** review 出力 JSON の `defect` finding に `quote` が必須化される
  (プロンプトに明記)。checkpoint の `Finding`/`LedgerEntry` に `quote`/`anchor_verified` が増えるが、
  いずれも `#[serde(default)]` の追加フィールドで、単一 reviewer 経路の checkpoint は
  byte-for-byte のまま(ADR 0023 の `reviewer_profile` 追加と同じ性質)。DB スキーマ変更は無い。
- **resume の文脈価値を捨てるコスト。** reviewer が fresh session になることで、毎ターン
  フルプロンプト再注入ぶんの token を払う。だがレビューは元々 diff 同梱の自己完結プロンプトで、
  resume が持ち込む文脈は薄く、旧 head の記憶という害のほうが大きい — 割に合う。異種モデル
  (ADR 0023)や小 context window プロファイルが増えるほど fresh 既定の安全余裕は効く。
- **stale 率という新しい CATCH 品質指標が出る(ADR 0026)。** reviewer が現物を読まずに古い記憶で
  指摘する頻度を可視化でき、編成やプロンプトの効き目を測れる。
- **原則「画面で成否裁定しない」は不変。** anchor 照合は引用文字列の実在確認(機械照合)であり、
  画面読解による成否判定ではない。completion contract の3条件にも触れない。
