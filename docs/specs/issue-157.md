# issue-157 spec — auto-merge の orchestrator 側マージモード(private + Free フォールバック)

## 背景と決定(要旨)

ADR 0003 は auto-merge を「GitHub ネイティブ auto-merge に arm するだけ、最終判定は GitHub(branch protection + required checks)に委ねる」と決めた。しかし **private + Free プランのリポジトリでは "Allow auto-merge" が有効化できず**(API PATCH が黙って無視される)、`enabled = true` は fail-fast(`auto_merge_allowed = false`)で必ず落ちる。

本 issue は、ネイティブ auto-merge が使えないリポジトリ向けのフォールバックとして **orchestrator 側マージモード** を追加する。sweep の適格判定(ブランチ・リンク・ラベル・opt-in・未解決スレッド)を共通のまま、GitHub が `MERGEABLE` を返す PR を meguri 自身が `merge_pr` で直接マージする。これは既存の「arm 時点で既に clean なら `merge_pr` で確定する」例外経路(ADR 0003 決定 1)の一般化である。

設計判断(arm-only を mode で二分する / orchestrator は meguri の PR 前検証を唯一のゲートとして受容する)は **ADR 0009(本 PR 同梱)** に置いた。本 spec はその実装計画に徹する。

## 主要な設計判断

1. **config 形は `mode = "native" | "orchestrator"`(enum)、既定 `"native"`。** `fallback = "orchestrator"` 案は退ける — 「使えなければ自動で切り替える」は meguri の「設定と現実の不一致は黙って吸収せず起動時に人間へ返す」方針(ADR 0003 決定 4)に反する。mode は明示的な選択であり、既存の `AutoMergeOptIn`(`label`/`all`)と同じ enum スタイルに揃う。

2. **orchestrator の sweep は適格条件 1〜6 を native と共有し、条件 7 だけを差し替える。** 分岐は `process_pr` の条件 7(現在は `validate_policy` + `arm`)で行う。orchestrator では policy 検証の代わりに `pr_mergeable` を読み、`MERGEABLE` なら `merge_pr(pr, strategy, head_sha)` で直接マージ、`CONFLICTING` は skip(conflict-resolver が所有)、`UNKNOWN` は skip(次 sweep)。arm もマーカーによる冪等性担保も無い — 即マージで PR が閉じ `list_open_prs` から外れるため、冪等性は forge state が担保する(ADR 0009 決定 1)。

3. **`validate_policy` を mode 対応にする。** orchestrator モードでは `auto_merge_allowed` と `protected_with_required_checks` を要求しない。ただし **設定 strategy がリポジトリで許可されているか**(`policy.allows(strategy)`)は残す — `gh pr merge --squash` は squash 不許可のリポジトリで失敗するため、orchestrator でも実在する前提。

4. **`require_branch_protection = true` + orchestrator は config `validate()` で弾く。** 既定値が `true` のため、orchestrator 利用者には `require_branch_protection = false` の明示を要求する。これは「サーバ側ゲート無し・meguri の検証だけがゲート」の受容を config 上に一行で残すためで、fail-fast 方針と一貫する(ADR 0009 決定 5)。
   - 代替案(reviewer 判断用): orchestrator では `require_branch_protection` を単に無視して警告に留める。silent に緩めることになり「no silent degradation」に反するため、本 spec は hard error を推す。

5. **doctor は orchestrator モード時に注意表示する。** 「サーバ側ゲートなし・meguri の PR 前検証(`check_command` + self-review)のみがゲート」を出力し、受容を運用者に想起させる(ADR 0009 決定 2 / issue の Notes)。

6. **merge-watch は orchestrator モードでは実質 no-op。** merge-watch は armed marker を持つ PR だけを watch する(`is_armed`)。orchestrator は arm しないので watch 対象が生まれない — 即マージにドリフト窓が無く、これは正しい。追加変更は不要。

## 変更箇所

### 1. `src/config.rs` — `mode` フィールド追加 + validate
- `AutoMergeMode { Native, Orchestrator }` enum(`#[serde(rename_all = "lowercase")]`、既存 `AutoMergeOptIn` と同型)を追加。
- `AutoMergeConfig` に `#[serde(default = "default_auto_merge_mode")] pub mode: AutoMergeMode`(既定 `Native`)を追加。`Default` 実装・`default_auto_merge_mode()` も追加。
- `Config::validate()` に: `am.mode == Orchestrator && am.require_branch_protection` を全プロジェクトの実効 `[pr.auto_merge]` について検査し、真なら `bail!`(`require_branch_protection = false` を促す文言)。

### 2. `src/engine/auto_merger.rs` — orchestrator 分岐
- `validate_policy(cfg, policy)` を mode 対応に:native は現行 3 点、orchestrator は strategy 許可のみ。
- `process_pr` の条件 7 を mode で分岐:
  - `Native` → 現行(`merge_policy` fetch → `validate_policy` → `arm`)。
  - `Orchestrator` → `pr_mergeable(pr.number)` を読み、`Mergeable` のときだけ直接マージするヘルパ(`merge_directly` 等)を呼ぶ。`Conflicting` / `Unknown` は silent skip。
  - 注意: orchestrator パスでは条件 5(head-armed マーカー読み)は不要(arm しないため)。ただし条件 6(未解決スレッド 0)は **維持** — 未解決スレッド = self-review が accept していない、の意で orchestrator のゲートの一部。
  - policy fetch は orchestrator でも strategy 検証のため一度だけ行う(既存の `policy: Option<MergePolicy>` キャッシュを流用)。
- 直接マージヘルパ:draft なら `mark_pr_ready` → `merge_pr(pr.number, strategy, head_sha)` → 監査用に merged コメント + `pr.automerge_merged` イベント emit。`--match-head-commit` により head 移動時は GitHub/fake が拒否(TOCTOU)。
  - **監査コメントは `merged_comment` を再利用しない。** 現行の `merged_comment` は本文に `armed_marker(head_sha)`(`<!-- meguri:automerge armed ... -->`)を含むが、orchestrator は arm もマーカーも作らない(主要判断 2・受け入れ基準 2・ADR 0009 決定 1)。そこで **マーカーを含まない別ヘルパ `orchestrator_merged_comment(strategy, head_sha)` を新設** し、orchestrator パスはこれを使う。marker を残さない理由は冪等性を marker ではなく forge state(即マージ→PR クローズ→`list_open_prs` から消える)で担保するため(ADR 0009 決定 1)。`pr.automerge_merged` イベントは native と共有で再利用する。
  - merge-watch は `ARMED_MARKER_PREFIX` を持つ PR だけを watch する(主要判断 6)。orchestrator の監査コメントにマーカーが無いことで、この invariant が保たれる。

### 3. `src/app.rs` — `auto_merge_preflight` の mode 対応
- `validate_policy` が mode を見るようになるため、preflight 側の呼び出しは基本そのままで通る。orchestrator では protection/auto-merge-allowed を要求しなくなることを確認。

### 4. `src/main.rs` — doctor(`check_auto_merge`)の mode 対応 + 注意表示
- `validate_policy` の mode 対応に追随。orchestrator の成功表示に「サーバ側ゲートなし・meguri の検証のみがゲート」の注意行を添える。

### 5. `docs/adr/0009-...md`(同梱・作成済み)/ `docs/adr/0003-...md` に追補(作成済み)

### 6. `README.md` / `README.ja.md` — Auto-merge 節
- `mode` の説明(native=既定/推奨、orchestrator=フォールバック)、config 例に `mode` 追加、「Free/private ではネイティブが使えない」経緯(ADR 0004 / 0009 参照)、orchestrator は「meguri の PR 前検証が唯一のゲート」である点を追記。

### 7. `tests/auto_merge_test.rs` — fake forge テスト
- orchestrator: 適格 PR で `pr_mergeable = Mergeable` → `merge_pr` が呼ばれ PR が merged になる(`set_pr_mergeable` / `merged` を検証)。監査コメントに `ARMED_MARKER_PREFIX`(arm マーカー)が **含まれない** ことを検証(`orchestrator_merged_comment` のユニットテスト)。
- ブロックラベル付き → マージされない。
- 未解決スレッドあり → マージされない。
- `CONFLICTING` → skip(conflict-resolver に委ねる、merged にならない)。
- `UNKNOWN` → skip(次 sweep 持ち越し)。
- `validate_policy` の mode 別ユニットテスト(orchestrator は auto_merge 不許可/protection なしでも OK、strategy 不許可は NG)。
- config: `mode` パース、`orchestrator` + `require_branch_protection = true` が `validate()` で弾かれる。

## 受け入れ基準

1. `[pr.auto_merge].mode` が `"native"`(既定)/`"orchestrator"` でパースでき、未指定時 native。
2. orchestrator モードで、適格かつ `MERGEABLE` の meguri PR が設定 strategy で直接マージされる(arm もマーカーも作らない — 監査コメントは `orchestrator_merged_comment` で `ARMED_MARKER_PREFIX` を含まない)。
3. orchestrator モードで `CONFLICTING` はマージされず(conflict-resolver 委任)、`UNKNOWN` は次 sweep に持ち越される。
4. orchestrator モードで、ブロックラベル・未解決スレッドのある PR はマージされない。
5. `meguri doctor` / `meguri watch` の fail-fast が mode 対応:orchestrator では "Allow auto-merge"・branch protection を要求せず、strategy 許可のみ検証する。native は現行どおり。
6. `mode = "orchestrator"` + `require_branch_protection = true` は config load 時に拒否される。
7. `meguri doctor` が orchestrator モード時に「サーバ側ゲートなし・meguri の検証のみがゲート」を注意表示する。
8. README(en/ja)の Auto-merge 節に mode と Free/private の経緯が追記される。
9. 上記を網羅する fake forge テストが通り、`cargo test` / `cargo clippy` がグリーン。
