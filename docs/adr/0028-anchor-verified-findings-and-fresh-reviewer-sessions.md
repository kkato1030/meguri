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

### 2. stale は1回だけ差し戻し、なお stale なら棄却する(needs-human に落とさない)

anchor 照合は **新規 finding**(まだ台帳に無い、`id` の無い finding)だけを対象にする。既存 id の
再リストは照合しない(初出時に照合済みで、fix でコードが正当に動いた結果 quote が消えることがある。
その解消/継続は台帳の ping-pong ロジックが裁く)。単一 reviewer / round 2+ の sequential review ターンで、
新規 finding に stale があれば **1回だけ**「anchor 照合に失敗した。対象 head を読み直して再レビューせよ
(該当 finding の一覧付き)」と差し戻す。

- **retry 状態は既存の corrective-turn(tree 汚し・id 不整合)とは別カウンタ**にする。現行実装の
  `corrective_turns` は tree/id/出力検証をまとめて数え2回目で `NeedsHuman` に昇格するが、anchor stale は
  **専用の1回リトライ**を持ち、その終端は **needs-human ではなく棄却**に固定する。検証順序は
  「tree/id(ハード契約違反、従来どおり)→ その後 anchor 照合(clean で valid な出力に対して)」とし、
  2系統は独立に高々1回ずつ差し戻す。
- 差し戻し後の再レビューで照合を通れば通常どおり台帳へ。
- 再レビューでもなお stale な新規 finding は **台帳に入れず棄却**する(fixer を回さない)。verified な
  finding だけで phase を続ける。これで「存在しない引用による偽の不収束 → needs-human」経路が閉じる。
- **drop が「解消」と誤読されない**ことを保証する。現行の台帳 fold は「再リストされなかった既存 entry =
  解消(Fixed/Waived)」と解釈するため、stale を findings から単に除くと未解決 entry を閉じてしまう。
  よって棄却は **新規 finding にのみ**適用し(除いても閉じる対象がない)、既存 entry には anchor 照合を
  かけない(前段の対象限定で担保)。fold に渡す再リスト集合を stale で削らないことを不変条件にする。

**round 1 の parallel reviewer では sequential な差し戻しをしない。** 各 `self-review#N` はすでに
fresh session(§3)で対象 head を読んでいるため round 1 の stale は稀で、reviewer 別の corrective-turn
retry を持つと fan-out の合流が非決定的になる。代わりに **union-merge 後の新規 finding をまとめて
anchor 照合し、stale を棄却**する(verified な他 reviewer の finding は影響を受けない)。決定的な手順で、
retry 単位の曖昧さ(どの N を再実行するか)を持ち込まない。バウンス付き1回リトライは sequential 経路
(単一 reviewer / round 2+)固有の挙動とする。

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
  両者とも `#[serde(default, skip_serializing_if = "Option::is_none")]`。`anchor_verified` は照合が
  実際に走ったときだけ `Some(true)` を入れ、それ以外(`anchor_verification` 無効時・decision 免除の
  非記録時)は `None` で **serialize されない**。これで「anchor 無効時は checkpoint が byte-for-byte
  不変」「旧 checkpoint も serde default で読める」を分けて満たす(常時 serialize される裸の `bool` では
  前者が崩れる、という指摘への対応)。
- **stale 率は単一の定義に固定する**(f6 の決定)。二重計上と母集団の曖昧さを避けるため、
  numerator/denominator を **1種類のイベント** `self_review.anchor_checked` から導く:
  reviewer ターンが anchor 照合を終えた時点で **ターンにつき1回だけ** emit し、payload に
  `{ round, reviewer_index, findings_total, stale_discarded }` を載せる(差し戻し中間状態では emit しない
  ので二重計上しない。parallel は reviewer ごとに1回、sequential は round ごとに1回)。
  **stale 率 = Σ`stale_discarded` / Σ`findings_total`**(照合した新規 finding のうち、逐語照合に失敗して
  棄却された割合)。`meguri stats review` はこの1イベントを合計して出し、CLI は
  「anchor stale: X.X%(棄却 N / 照合 M)」の形で表示する。台帳フィールドは監査/表示専用で stats の
  母集団にしない(「イベント発火点 = stats のソース」という既存イディオム)。

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
