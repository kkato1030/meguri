# ADR 0025: guard(pr-reviewer) を安全 tripwire にする — advisory は止めず、blocking は限定カテゴリのみ

- Status: proposed
- Date: 2026-07-20
- Issue: #228
- 関連: ADR 0008(対称 review loop・pr-review gate §5)・ADR 0006(AI 実装レビューの内部ループ化)・ADR 0013/0014(spec_fixer)・ADR 0022(severity 不採用)・ADR 0009(parked-review signal)

## Context

guard(pr-reviewer)は今「品質ゲート」として動き、レビューの完走を妨げすぎている。

- guard verdict の実測は **clean 67 / findings 69**。レビューした PR の約半分が止まる。
- Impl PR の findings には修正ループが無い。`fixer`(ADR 0006)は人間・外部 bot のスレッド
  しか拾わないので、auto_merger の pr-review gate で findings は**無条件に needs-human 直行**する
  (`pr_review.escalated` 26件、確認したサンプルはすべて impl)。
- 現行の impl guard プロンプトは "correctness, completeness, and fit with conventions" を見る。
  これは self-review(内部ループ)がすでにやっている品質レビューの二度目である。
- 一方 Plan の findings は spec_fixer(ADR 0013/0014)の自動修正ループが捌く。escalation は
  impl に集中している。

self-review は品質を収束させる「直す側」だ。guard がもう一度品質を見て、しかも直す手段を
持たないまま止めるのは、人間ゲートの安売りになっている。

## Decision

**guard を品質ゲートから「安全 tripwire」に退かせる。impl では advisory は止めず、
blocking は閉じたカテゴリに該当するときだけとする。Plan は触らない。**

| 段 | 役割 | 止める条件 |
|---|---|---|
| self-review(内部ループ) | 品質の収束 — 直す側 | 挙動ベース(ADR 0022) |
| guard(独立レビュー) | 安全の tripwire — 止める側 | blocking カテゴリのみ |

### 1. verdict を `clean | advisory | blocking` の三値にする

guard の消費者は auto_merger の「進める / 止める」分岐なので、「止めるか否か」の二値はまさに
挙動分岐である。序数 severity(low/high)は ADR 0022 どおり導入しない。三値のうち止めるのは
blocking だけで、clean と advisory はどちらも auto-merge を進める。

### 2. blocking は閉じたカテゴリ列挙で定義する

blocking にできるのは次の4カテゴリを**明示できるとき**だけとする:

- **security** — 脆弱性・secrets 露出
- **data-loss** — データ喪失・不可逆な外部作用
- **cost** — 暴走課金
- **performance** — 破滅的な性能劣化

プロンプトに「該当カテゴリを明示できない限り advisory。疑わしいだけなら advisory」と明記する。
これは Google Tricorder の「program analysis の指摘はデフォルト advisory、blocking にできるのは
実効偽陽性率が十分低い検査だけ」と同型で、CI の required check / neutral check の使い分けと同じだ。
recall を上げたいなら blocking を増やすのではなく記録・表示を良くする。

### 3. advisory は止めない

advisory の指摘は PR 本文の折り畳み `<details>` に記録し(ADR 0008 の会話タイムライン外原則
そのまま)、commit status は **success** にして auto-merge を続行する。人間は merge 後でも読める。

### 4. gate 意味論の変更は settle の status 分岐に閉じ込める(ADR 0008 §5 の部分置き換え)

auto_merger の pr-review gate は今も commit status(success/failure/pending)だけを読む。したがって
gate 側のコードは変えない。verdict → status の対応を settle で変えるだけで「blocking のみ escalate」を
満たす:

- **clean / advisory → success**(auto-merge 進行)
- **blocking → failure**(auto_merger が needs-human へ park。escalation は現状維持)

blocking → needs-human 直行は現状維持とする。impl 側に fixer ループは足さない。真にやばいものを
AI が自動修正して通すのは tripwire の自殺であり、稀になった blocking こそ人間ゲートの正しい使い所だ。

### 5. Plan は触らない

Plan の guard は品質ゲートのまま残す。spec_fixer の自動修正ループが「実装を妨げず完走」を
すでに実現しているからだ。Plan の settle は今のまま — clean → spec-ready、非 clean → failure を
維持して spec-reviewing に留め、spec_fixer に委ねる。Plan は advisory / blocking を区別せず、
非 clean はすべて従来の findings と同じ扱いにする(blocking カテゴリも要求しない)。

## Consequences

- **guard が「止める側」から「稀に止める側」に退く。** impl の advisory findings は needs-human に
  ならず auto-merge まで通る。人間ゲートは真に危険な blocking にだけ絞られる。
- **gate のコードは不変。** verdict → status の対応を settle が持ち替えるだけで、auto_merger /
  spec_fixer / ci_fixer は変わらない(`meguri/*` status 除外・spec_fixer の failure 駆動もそのまま)。
- **verdict コントラクトが広がる。** `.meguri/review.json` の verdict が三値になり、blocking は
  カテゴリ列挙(impl のみ必須)を伴う。events(`pr_review.posted`)にも verdict と categories が乗り、
  blocking 率の推移を追える。効果測定の狙いは blocking 率が数%台に落ちること。
- **advisory は本公開される。** self-review が見ていない観点を guard が advisory で拾っても、それは
  止めずに `<details>` に残るだけになる。真にやばい観点を blocking カテゴリに閉じたのは意図的で、
  取りこぼしのコストより「半分が止まる」コストを重く見た判断である。
- **injection 面は ADR 0024 のまま。** guard の review body は spec_fixer(plan)の fix prompt に
  入るが、impl 側は本 ADR で advisory を fix ループに繋がない(記録のみ)ので面は広がらない。
