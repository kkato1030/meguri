# ADR 0031: blocking finding は現物引用で機械照合し、reviewer ターンは fresh session を既定にする

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

- meguri は finding を台帳に畳み込む前に、`quote` が **対象 head の該当ファイル(`path`)に逐語で
  存在するか**を照合する。存在しなければその finding は `stale`(照合失敗)とみなす。
- 照合は substring 一致で行う。`line` は人間と fixer のための位置ヒントであり、照合の一致条件には
  含めない(古い行番号で正しい引用を落とさない)。
- **`path` は信頼境界の外にある入力**として扱う。reviewer が書いた `path` を repo-relative に正規化し、
  絶対パス・`..`・worktree を抜ける symlink はすべて **照合失敗(stale)** に倒す。読む対象は
  working tree のファイルではなく **clean な HEAD の tracked blob**(`git show HEAD:<path>` 相当、
  `gitops` に集約)とする — 未 track/改変ファイルに騙されず、reviewer/fixer のプロンプトへ worktree 外の
  内容(例: `/etc/hosts`)が流れ込む経路も塞ぐ。
- **`decision` 型 finding は照合対象外**(ADR 0022)。decision は「A か B かを決めて spec/impl に
  **追記**せよ」型で、既存コード文字列を指すとは限らないため、`quote` は任意、照合は skip する。

### 2. stale は1回だけ差し戻し、なお stale なら「新規は棄却・再リストは Waived 化」に振り分ける

anchor 照合は、その review ターンが出した **すべての `defect` finding(新規・既存 id の再リスト双方)** を
対象にする。既存 id の再リストを照合から外すと、旧 head の引用を同じ id で再主張する経路(まさに #231 の
型)が残り、stale なまま fixer と再レビューを往復して ping-pong → needs-human に到達しうるためだ。
単一 reviewer / round 2+ の sequential review ターンで stale があれば **1回だけ**「anchor 照合に失敗した。
対象 head を読み直せ。**修正でコードが消えた finding は drop、まだ残る concern は現 head の文字列で引用し直せ**」
と差し戻す(該当 finding の一覧付き)。

- **retry 状態は既存の corrective-turn(tree 汚し・id 不整合)とは別カウンタ**にする。現行実装の
  `corrective_turns` は tree/id/出力検証をまとめて数え2回目で `NeedsHuman` に昇格するが、anchor stale は
  **専用の1回リトライ**を持ち、その終端は **needs-human ではなく下記の振り分け**に固定する。検証順序は
  「tree/id(ハード契約違反、従来どおり)→ その後 anchor 照合(clean で valid な出力に対して)」とし、
  2系統は独立に高々1回ずつ差し戻す。
- 差し戻し後の再レビューで照合を通れば通常どおり台帳へ(`anchor_verified = Some(true)`)。
- 差し戻し後もなお stale な finding は、**新規か再リストかで扱いを分ける**(f4 と f9 の両立):
  - **新規 finding(台帳に無い)が stale** → **台帳に入れず棄却**(fixer を回さない。閉じる対象が無いので
    omission 誤読も起きない)。これで「存在しない引用による偽の不収束 → needs-human」経路が新規側で閉じる。
  - **既存 id の再リストが stale** → その entry を **`status = Waived` に落とし**、`anchor_verified = Some(false)` と
    システム由来の `waive_reason`(例: 「anchor 照合失敗: 現 head に該当引用なし」)を付ける。`Open` のままには
    **しない**。理由は rollback 安全性(下記 f-259-1): `Open` のまま残すと、`self_review_ledger` を正として読む
    **#247 より前(ADR 0022 以降)の ledger-aware binary** が、未知の `anchor_verified` を serde で無視して
    その `Open` entry をそのまま fixer に渡し、元の ping-pong を再発させる。`Waived` はどの ledger-aware binary も
    「解消済み・非 actionable」と解釈する終端 status なので、rollback しても fixer に渡らない。
  - この扱いは f4(drop == 解消 の誤読)と衝突しない: omission(再リストされなかった → 著者 disposition で
    自動解消)の暗黙経路ではなく、**anchor 照合失敗を理由に明示的に waive し、`anchor_verified=Some(false)` と
    システム reason で印を付ける**。fresh session で読み直してなお現 head に引用できない finding は、実在の
    concern を提示できていないので自動修正の入力から外す、という設計判断を可視な形で記録する。
  - `anchor_verified = Some(false)`(= Waived)の entry は open 数に数えないので phase は普通に収束でき、
    needs-human に落ちない。publish 時(clean でも cap→final-fix でも)にこの entry を PR の `<details>` に
    「anchor 未照合(照合失敗)」として出し、human merge gate が見る(透明性)。`anchor_verified` は
    `status` と独立なフィールドなので、この描画は status に関係なく `Some(false)` を鍵に行う。

- **この `Waived` は「system-waive」であり、本 ADR が ADR 0022/0026 の `waived` 意味論を明示的に精緻化する
  (f-259b-1)。** ADR 0022 は `waived` を「作者が理由付きで同意しない状態」と定義し、ADR 0026 はそれを
  `waive_rate` として捕捉と別に数える。本 ADR で台帳の `Waived` は今後2種を含むことになる:
  (a) **author-waive**(ADR 0022 どおり。作者 disposition 由来、`anchor_verified` は `None`)、
  (b) **system-waive**(anchor 照合失敗の再リスト。`anchor_verified = Some(false)`、system 由来の `waive_reason`)。
  両者を分ける鍵は `anchor_verified`。ADR は追記式なので 0022/0026 の本文は改めず、精緻化は本 ADR に記録する。
  **「作者が拒否した」を意味する全 consumer は `anchor_verified = Some(false)` を除外する**こと。特に ADR 0026 の
  `waive_rate` は **author-waive のみ**を数える(system-waive を含めない)。`waive_rate` は本 issue では
  未実装(将来 phase)なので、実装時にこの除外を必須とすると前もって仕様化する。**rollback で `status` だけを
  読む旧 binary は system-waive を author-waive と数え得るが、これは実行時契約(ping-pong 再発なし)ではなく
  統計上の一時誤差**であり、run 完了までの限定的劣化として許容する(fixed の numerator は汚さない — system-waive は
  `Fixed` にしないため ADR 0026 の捕捉数には混じらない)。

**round 1 の parallel reviewer は merge の前に reviewer 別で照合する(f8: 実行単位と統計単位を一致)。**
union-merge すると reviewer 境界が消えて `reviewer_index` への帰属が付けられないので、**各 `self-review#N` の
findings をその reviewer 単位で照合し、stale を棄却してから union-merge** する(verified な他 reviewer の
finding は影響を受けない)。round 1 は全 finding が新規なので棄却で済み、reviewer 別の corrective-turn retry
(どの N を再実行するかの非決定性)は持ち込まない。各 reviewer が fresh session(§3)で head を読むので
round 1 の stale はそもそも稀。バウンス付き1回リトライは sequential 経路(単一 reviewer / round 2+)固有とする。

**anchor confirmation の overrule findings も merge 前に照合する(f11)。** parallel round で `needs_human`
verdict が出ると単一 anchor reviewer が再検討し(ADR 0023 §2)、overrule 時にその追加 findings が
`merge_reviews` の `extra` として union に**後から**足される。ここが reviewer 別照合を素通りする穴になるので、
**overrule findings も anchor reviewer の turn 内で照合してから merge に渡す**(「すべての `defect` finding を
照合する」契約を全経路で守る)。イベント帰属は **専用の anchor index**(`reviewer_index` = anchor の枠。
parallel 枠とは別の固定値、例: `self-review-anchor` を表す index)とし、`self_review.anchor_checked` を
anchor confirm turn につき1回 emit する。

### 3. reviewer ターンは fresh session を既定にする(resume は fixer 系のみ)

reviewer ロール(`self-reviewer` / `pr-reviewer`)のターンは、毎ターン **素の spawn + フルプロンプト
再注入**で始める。session id を resume に読まないだけでは不十分な点に注意する。

- **生存 pane を先に畳んでから spawn する。** 現行 `ensure_pane` は session id を読む前に、lane の
  生きた pane をそのまま adopt する。keep-pane 設定では前ターンと同じ pane・同じ session が残り、次ターンは
  新規 spawn ではなく同じ pane への `send_line` になってしまう。よって reviewer ターンは spawn 前に
  その lane の生存 pane を release/kill(advisor の「捨てて張り直す」再 embody と同じ経路)し、
  resume 引数なしで素の spawn を行う。**pane モード・direct モードの両方**でこの挙動(前ターンの
  session に接続しない)を検証する。
- author lane(worker/planner/spec-worker と、そこへ相乗りする fixer 系 = fixer/spec-fixer/
  ci-fixer)は従来どおり生存 pane を adopt し resume する — fix は直前の実装文脈が効くからだ。
- これで #231 の構造原因(旧 head の記憶が現物より強い)を根から絶つ。#231 の実インシデントは
  pr-reviewer(prose findings)側だったが、**fresh session 既定はロープ横断の session lifecycle
  変更**で、内部 self-review と外部 pr-reviewer の両方に効く。
- observability のための session id 保存自体は残す(`meguri ps` / 診断)。resume の**読み取り**を
  reviewer ロールで止めるだけで、ADR 0004 の lane = issue スコープの resumable context という
  枠は author/fixer 側で不変。

### 4. anchor 照合の結果を台帳と統計に出す(ADR 0026 の CATCH 品質)

- `LedgerEntry` に `anchor_verified: Option<bool>` を、`Finding` に `quote: Option<String>` を足す。
  両者とも `#[serde(default, skip_serializing_if = "Option::is_none")]`。値の意味は3値:
  `None`(照合が走っていない — `anchor_verification` 無効時・decision 免除)/`Some(true)`(照合を通った)/
  `Some(false)`(差し戻し後もなお照合失敗の**再リスト**。同時に `status = Waived`)。`anchor_verification`
  無効時は全 entry が `None` で **serialize されない**ので checkpoint は byte-for-byte 不変、旧 checkpoint も
  serde default で読める。
- **`anchor_verified` は「監査/表示専用」ではなく、`status` を補助する表示/統計フラグである(f14)。**
  actionability(fixer が触るか)の制御は **`status` 自身**が担う: stale な再リストは `status = Waived` に
  なるので、`fix_turn` が拾う `status == Open` の集合から自然に外れる(旧 binary も同じ)。`anchor_verified`
  はそれ自体で actionability を変える必要がなく、**(1) 表示(`<details>` に「anchor 未照合」と出す鍵)、
  (2) stats(この entry を stale と識別)** の2つに効く。境界: **open/fixed/waived の遷移は `status` が持ち、
  `anchor_verified` は「その waive が anchor 照合失敗由来か」という由来ラベル + 表示/統計の鍵**。これで
  「監査専用 vs actionability 制御」という以前の二枚舌(f14)を解き、かつ actionability を status 一本に集約して
  rollback 安全性(前 §2)も同時に満たす。
- **stale 率は単一の定義に固定する**(f6/f8 の決定)。二重計上と母集団の曖昧さを避けるため、
  numerator/denominator を **1種類のイベント** `self_review.anchor_checked` から導く。発火単位を照合単位に
  一致させる: **anchor 照合を実行した reviewer ターンにつき1回だけ** emit する(parallel は各 `self-review#N`
  が merge 前に自分の findings を照合するので reviewer ごとに1回・`reviewer_index` 付き、sequential は
  round ごとに1回。イベントは二重に出さない)。payload は `{ round, reviewer_index, findings_total, stale_count }`。
- **その1イベントのカウントは、そのターンの全 attempt(初回 + anchor 差し戻し後の再試行)を通算する
  (f-259b-2)。** イベントは依然ターンにつき1回だが、`findings_total` はそのターンで照合した `defect` finding の
  **延べ数(各 attempt 合算)**、`stale_count` は照合に失敗した延べ数(新規棄却ぶん + 再リスト Waived 化ぶん)。
  こうしないと「初回 stale → 差し戻し後 clean」のターンが最終試行だけ数えられて `stale_count=0`(stale 率 0%)に
  なり、「reviewer が現物に無い引用を出した頻度」という定義から stale 出力が漏れる。通算なら
  初回 stale(findings=1, stale=1)+ 再試行 clean(findings=1, stale=0)= `findings_total=2, stale_count=1`
  となり stale が1回数えられる。各 attempt の stale ⊆ その attempt の findings なので `stale_count ≤ findings_total`
  が保たれ、率は常に [0,1]。差し戻しの無い parallel / anchor_confirm は attempt が1つなのでその1回分。この
  「初回 stale → 再試行 clean」ケースを unit test に持つ(受入基準)。
- **stale 率 = Σ`stale_count` / Σ`findings_total`**。`meguri stats review` はこの1イベントを合計して出す。
  **分母 M = Σ`findings_total` が 0 のとき**(全 clean ターン・再リストのみ・有効化直後など)は
  **ゼロ除算を避け、CLI は `N/A(照合 finding 0件)` と表示**する(0.0% ではない)。`findings_total = 0` でも
  イベント自体は emit し(照合を走らせたターン数 = coverage を数えられる)、率だけを N/A にする。M=0 の
  unit test を持つ(受入基準)。stale 率の分子・分母は**イベントから**取り、台帳の `anchor_verified` は
  stats の母集団にしない(「イベント発火点 = stats のソース」という既存イディオム。フィールド自体は上記の
  表示・統計識別に使う)。
- **anchor 統計は terminal event から独立に集計する(f-259-2)。** 既存の `review_stats` は terminal event
  (clean/unconverged/…)だけを読み、terminal が0件のグループを母集団から落とす。この経路に anchor 統計を
  相乗りさせると、anchor 照合後に pane 停止などで **terminal event が出なかった run の `anchor_checked` が
  coverage から消える**(仕様が約束する coverage/N/A と矛盾する)。よって anchor 統計は
  `self_review.anchor_checked` **だけ**を集計母集団にし、terminal event の有無に依存させない。集計は
  既存の review 統計とは別のロールアップ(別 struct・別セクション表示)として持つ。これは ADR 0020/0026 の
  「計測は実行時契約を変えない・イベントを源にする」立て付けの範囲内。

## スコープ(この ADR が**やらないこと**)

- **pr-reviewer の prose findings の構造化・anchor 照合はやらない。** pr-reviewer は今も
  `review` 散文を PR body に畳む契約で、per-finding の構造 anchor を持たない。ここに anchor 照合を
  入れるには pr-reviewer の出力契約を構造化 finding へ作り替える別作業が要る。#247 では
  **pr-reviewer には fresh session だけ**を効かせ、構造 anchor 照合は内部 self-review 台帳に限る。
  pr-reviewer の finding 構造化は将来の follow-up。

## Consequences

- **機械照合できる判定が人間ゲートから外れる。** 「存在しない引用」による偽の不収束が
  needs-human に到達しなくなる(差し戻し1回 → クリーンなら通過、なお stale なら新規は棄却・再リストは
  system-waive)。**「静かに」ではない**: 棄却/waive はどちらも anchor stale 統計(§4)に数え、再リストの
  system-waive は PR の `<details>` にも出す。棄てた事実は統計と PR 本文の両方に残る。
- **reviewer コントラクトが広がる。** review 出力 JSON の `defect` finding に `quote` が必須化される
  (プロンプトに明記)。checkpoint の `Finding`/`LedgerEntry` に `quote`/`anchor_verified` が増えるが、
  いずれも `Option` + `skip_serializing_if = "Option::is_none"` の追加フィールドなので、旧 checkpoint は
  serde default で読め(後方互換)、`anchor_verification` 無効時は None のまま serialize されず
  byte-for-byte 不変が保たれる(§4)。anchor 照合が走る内部 self-review 経路では、new なので
  「未変更」ではなく「照合結果を記録した新しい表現になる」— byte-for-byte 不変を主張するのは
  **anchor 無効時**に限る。DB スキーマ変更は無い。
- **resume の文脈価値を捨てるコスト。** reviewer が fresh session になることで、毎ターン
  フルプロンプト再注入ぶんの token を払う。だがレビューは元々 diff 同梱の自己完結プロンプトで、
  resume が持ち込む文脈は薄く、旧 head の記憶という害のほうが大きい — 割に合う。異種モデル
  (ADR 0023)や小 context window プロファイルが増えるほど fresh 既定の安全余裕は効く。
- **stale 率という新しい CATCH 品質指標が出る(ADR 0026)。** reviewer が現物を読まずに古い記憶で
  指摘する頻度を可視化でき、編成やプロンプトの効き目を測れる。
- **原則「画面で成否裁定しない」は不変。** anchor 照合は引用文字列の実在確認(機械照合)であり、
  画面読解による成否判定ではない。completion contract の3条件にも触れない。
