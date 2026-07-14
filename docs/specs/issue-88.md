# spec: triage (3/3) v2 auto — 閾値超えを本ラベルへ昇格し自動着手

対象 issue: #88 / ブランチ: `meguri/88-triage-3-3-v2-auto-meguri-read-f558ed`

> このспец は使い捨ての足場。実装が入ったら消える。恒久的な設計判断は
> **ADR 0017**(`docs/adr/0017-triage-auto-promotes-real-labels-guarded.md`)へ、
> ここには実装を収束させるための受け入れ基準・触るファイル・決定事項だけを置く。

## spec 深度: design(+ migration/rollback 必須)

理由: 変更は **任意の open issue に本ワークフローラベルを付け、worker/planner を無人で起動する**。
不確実性は中だが blast radius が広く、誤昇格は planner 消費・PR 生成という **実質不可逆な作用**へ
直結する。適応 spec の veto ルール(不可逆な運用リスク → migration & rollback 必須)に該当するため
design tier とし、rollback を必ず書く。設計判断は issue が要求するとおり ADR 0017 に切り出した。

## ゴール(この issue のスコープ)

`[triage] mode = "auto"` のとき、v0/v1 の推薦のうち閾値を超えたものを本ラベルへ昇格し、既存の
worker / planner ループへ自動投入する。read-only(v0)→ 提案(v1)→ **自動着手(v2)**の最終段。

## 受け入れ基準

- `mode = "auto"` かつ `confidence >= confidence_threshold`(既定 0.7)かつ推薦種別が `apply` に
  含まれるときだけ、対象 issue に本ラベルを付与する:`ready` → `meguri:ready`、`plan` →
  `meguri:plan`。**`ready`/`plan` の2フェーズラベルのみが昇格対象**(ADR 0005 の2軸モデル)。
- **`needs-human` は auto 昇格しない**(据え置き)。`meguri:needs-human` はフェーズラベルではなく
  ボール所在ラベルで、フェーズラベルに重ねてのみ付く(ADR 0005)。未トリアージ issue(フェーズラベル 0 個)へ
  単独で付けると cleaner が anomaly として報告し、2軸不変条件も壊す。かつ ADR 0005 では「無ラベル = 未トリアージ
  = 人間が判断」なので、`needs-human`(要件不足で人間判断が要る)は本来「無ラベルのまま据え置き」と等価。
  issue #88 は `needs-human` → `meguri:needs-human` を挙げるが、ここは意図的に逸脱する(下記「主要な決定」)。
- 昇格のたびに **理由コメント**を1件残す(confidence / 複雑度 / 根拠 / 差し戻し手順)。
- 閾値未満・`apply` 外・`needs-human` / `skip` / `hold` は据え置き(per-issue の書き込みなし。中央レポートには載る)。
- **既定 `mode = "off"`**。auto は明示設定でのみ動く(オプトイン)。
- **`confidence_threshold` は `0.0..=1.0` に validation する**。範囲外(負値 → 全昇格の暴走側、
  `1.0` 超 → 黙って全停止)を config load 時に `bail!` で弾く。誤設定を危険側にも不可解な停止にも倒さない。
- **人間ラベル非上書き / 却下尊重**: 既に本ワークフローラベル・`meguri:hold` があれば触らない
  (書き込み直前に fresh に再読)。人間が昇格した本ラベルを剥がしたら、内容が変わるまで再昇格しない。
- **advise→auto 移行と却下尊重を両立**(マーカーの適用レベル + 提案ラベルの有無で判定):
  - **real マーカーが現ハッシュと一致** → 抑止(昇格済み、または昇格後に人間が剥がした = 却下)。内容が
    変わるまで触らない。
  - **proposal マーカーが現ハッシュと一致**:提案ラベルが **まだ付いている**(保留中)なら auto では候補/
    再走査対象にして real 昇格へ escalate(ADR 0017 の移行要件)。提案ラベルが **外されている**(人間が却下
    した痕跡)なら抑止(issue #88 の「剥がした痕跡があれば触らない」)。
  - `report`/`advise` の既存挙動は不変(同一ハッシュのマーカーはレベルを問わず再提案を抑止)。
- **レート制限**: `max_actions_per_tick`(既定 3)で 1 tick の本ラベル付与数を上限。積み残しは
  マーカーの `backlog=1` で次スイープを起動。
- **bot ループ防止**: 昇格済み issue は本ラベルを持つため再トリアージ候補から外れる(別マーカー不要)。
- **監査**: 昇格ごとに `triage.promoted` イベントを emit。`meguri logs triage.promoted` で追える。
- 中央レポート issue は auto モードでも従来どおり毎スイープ全上書き。フッターに auto の挙動と
  差し戻し方法を書く。
- `off` / `report` / `advise` の既存挙動は不変(回帰なし)。

## 触るファイル

- `src/config.rs`
  - `TriageConfig` に `confidence_threshold: f64`(既定 0.7)と `apply: Vec<TriageAction>`
    (既定 `["ready"]`)を追加。`apply` は `ready|plan` のみを認識する typed enum の集合が望ましい
    (`needs-human`/`skip`/`hold` は昇格対象になる本ラベルを持たないので受け付けない。未知値は parse error に
    して誤設定を早期に弾く)。両方 `#[serde(default = ...)]`。
  - `Config::validate()`(1362 行〜)の `for p in &self.projects` ループに `validate_triage(p)` を追加し、
    `triage_for(p).confidence_threshold` が `0.0..=1.0` 外なら `bail!`(既存の `validate_cadence` /
    `validate_schedules` と同じ形)。
  - `INIT_TEMPLATE` にコメントで `[triage] mode = "auto"` の例を追記(任意だが推奨)。
- `src/engine/triage.rs`
  - `discover()` の mode ゲートに `Auto` を追加(現状 `Report | Advise`)。
  - `advise_backlog_changed` の mode ゲートを `Advise | Auto` に(内容ドリフト再走査シグナルは auto にも要る)。
  - `settle()`: `mode == Auto` のとき `apply_advise` の代わりに新 `apply_auto`(または `apply_advise` を
    汎用化)を呼ぶ。
  - 新昇格パス `promote_one`(v1 の `propose_one` を土台に):`confidence_threshold` と `apply` ゲート、
    `ready`/`plan` → `meguri:ready`/`meguri:plan` のマッピング(`needs-human` は昇格しない — `real_label()` は
    `NeedsHuman` も map するが promote 経路では ready/plan だけを扱う)、既存 `meguri:triage-*` 提案ラベルの除去、
    本ラベル付与、理由コメント(昇格マーカー付き)、`triage.promoted` emit。fresh 再読で engaged なら無操作。
    **ハッシュ一致スキップは real マーカーのときだけ**(proposal マーカー一致 + 提案ラベル健在なら昇格へ進む。
    提案ラベルが外れていれば却下として無操作)。
  - 冪等マーカー拡張:`advise_marker` に適用レベル(`applied=proposal|real`)を足し、`parse` を対応。
    旧マーカー(レベル欠落)は `proposal` として後方互換に解釈。
  - **候補化/再走査 signal を marker-level + 提案ラベル aware にする**:`content_changed_since_advise`
    (`gather_candidates` 用)と `marker_drifted`(`advise_backlog_changed` 用)は現状ハッシュしか見ないので、
    このままだと advise 済み(proposal マーカー・同一ハッシュ)の issue が auto でも候補にも再走査 signal にも
    ならず `promote_one` へ届かない。auto モードでは上記「受け入れ基準」の判定に合わせる:proposal マーカー
    一致 + 提案ラベル健在 → eligible、real マーカー一致または提案ラベル外れ(却下)→ 抑止。`report`/`advise`
    は現状維持。
  - `render_report`: auto 用フッター分岐(昇格済み行の表示、差し戻し手順)。
- ADR: `docs/adr/0017-triage-auto-promotes-real-labels-guarded.md`(本 PR で作成済み)。

新しい forge メソッド・新ラベル定数は不要(本ラベルは既存、`add_label`/`remove_label`/`comment`/
`issue_comments` も既存)。

## 主要な決定(レビューで確認したい点)

1. **`apply` の shipped default = `["ready"]`(推奨)。** issue のスコープ本文は既定案として
   `["ready", "plan"]` を挙げるが、同 issue の段階的ロールアウトは「まず `ready` から、`plan` は
   信頼を積んでから」と述べる。既定は安全側に寄せるべきなので `["ready"]` を推す(ADR 0017 決定2に
   根拠)。**要合意**:issue の文面どおり `["ready", "plan"]` にするか。
2. **`needs-human` は auto 昇格対象から外す(issue #88 から意図的に逸脱)。** issue は
   「needs-human 推薦は meguri:needs-human へ」と述べるが、`meguri:needs-human` はボール所在ラベルで
   フェーズラベルに重ねてのみ付く規約(ADR 0005)。未トリアージ issue へ単独付与するとフェーズラベル 0 個の
   anomaly になり、cleaner が乖離報告し、2軸不変条件も壊す。さらに ADR 0005 では「無ラベル = 未トリアージ =
   人間が判断」なので、`needs-human` は「据え置き(無ラベルのまま)」が意味的に等価。よって `apply` は
   `ready|plan` のみ認識する。**要合意**:この逸脱でよいか(代替は「フェーズ + ボールの複合付与」まで設計
   することだが、over-engineering と判断)。
3. **冪等マーカーは v1 を拡張(別立てにしない)。** 1 issue = 最新の triage マーカー1つが真実。
   `applied` レベルで proposal/real を区別。auto では **real 一致だけが抑止**、proposal 一致は提案ラベルが
   健在なら escalate・外れていれば却下として抑止 — これで advise→auto 移行と却下尊重を両立(ADR 0017 決定3)。
   注意:v1 の「マーカー一致なら候補化/再走査も止める」を auto にそのまま流用すると proposal 済み issue が
   `promote_one` へ届かないので、候補化/再走査 signal 側も同じ判定に揃える(上記「触るファイル」)。
4. **昇格済み除外は本ラベル自身で足りる**(別マーカー不要、ADR 0017 決定4)。

## migration & rollback

- **永続状態への影響**: forge 上の issue ラベル/コメントのみ。ローカル DB スキーマ変更なし。config は
  追加キー2つ(既定値ありなので既存 config は無改変で動く)。マーカー形式は後方互換(旧 = `proposal`)。
- **段階導入**: `off`(既定)のままなら挙動不変。操作者が `auto` + `apply = ["ready"]` で開始し、
  `meguri logs triage.promoted` で誤トリアージ率を観測 → 信頼が積めたら `plan` 追加や閾値調整。
- **rollback**:
  - 単一の誤昇格 → 本ラベルを剥がす(未着手なら差し戻し完了。着手済み=`meguri:working`なら run を
    stop するか `meguri:hold`)。可逆性は polling 1 周期ぶんのベストエフォート(ADR 0017 帰結)。
  - auto 全体を止める → `mode` を `advise` / `off` へ戻す(次スイープから昇格しない)。
  - 特定パターンの誤検知 → `triage.ignore`。

## observability

- `triage.promoted` イベント(issue / recommendation / label / confidence)を昇格ごとに emit。
  `triage.advised`(v1)とは別イベントにして auto の作用を区別できるようにする。
- 既存の `triage.reported` / `triage.claimed` は不変。中央レポートに昇格済み行と backlog 表示を残す。

## test strategy

- 単体(`src/engine/triage.rs` の `#[cfg(test)]`):
  - `confidence_threshold` 未満は昇格しない / 以上は昇格する。
  - `apply` 外の種別(既定で `plan`)は昇格しない。
  - `needs-human` / `hold` / `skip` は本ラベルへ昇格せず無操作(`needs-human` を単独ボールラベルとして
    付けない = フェーズラベル 0 個の anomaly を作らない)。
  - engaged(本ラベル・`meguri:hold`)issue は fresh 再読で無操作(人間ラベル非上書き)。
  - `confidence_threshold` の config validation:`0.0..=1.0` は通り、負値・`1.0` 超は `bail!`
    (`Config::validate` のテスト側に追加)。
  - 却下尊重:real マーカーがハッシュ一致なら再昇格しない / 内容が変われば貼り直す。
  - **advise→auto 移行**:proposal マーカー・同一ハッシュ・提案ラベル健在の issue が、auto では候補化され
    (`content_changed_since_advise` / `marker_drifted` が eligible を返す)`promote_one` まで届いて real 昇格
    する。提案ラベルが外れている(却下)同型の issue は抑止される。
  - マーカー拡張の roundtrip(旧マーカー = `proposal` 後方互換)。
  - `max_actions_per_tick` 超過で backlog=1、レポートフッター表示。
  - auto フッターが本ラベル・差し戻し手順に言及。
- 統合(`tests/*.rs`、`FakeForge`):auto モードで閾値超え推薦が本ラベル + 理由コメントとして付与され、
  閾値未満が据え置かれ、`triage.promoted` が記録されることを FakeForge の呼び出し記録で検証。
- 実装後に `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
  `cargo nextest run` / `cargo test --doc` を通す(CI と同じ並び)。
