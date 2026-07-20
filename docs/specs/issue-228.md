# spec: guard(pr-reviewer) を安全 tripwire 化する(issue #228)

> 使い捨ての足場。恒久的な設計判断は ADR 0025 に、gate 意味論の置き換えは ADR 0008 §5 の
> 部分置き換えとして ADR 0025 に置いた。実装が landed したらこの spec は消える。

## spec 深度の理由

**design spec** を選ぶ。guard verdict は agent が書き auto_merger が読む**契約(schema)**で、
gate の意味論(advisory → success)も変える。持続 state の DB 変更は無いが、実行中 run の
checkpoint JSON(`verdict` フィールド)の互換が絡むため、veto rule(schema / public contract に
触れる)により migration & rollback を必須とする。

## ゴールと非ゴール

- **ゴール**: impl guard を「品質ゲート」から「安全 tripwire」に退かせる。advisory は止めず
  auto-merge を通し、blocking(閉じた4カテゴリ)だけを従来どおり needs-human に park する。
- **非ゴール**: Plan guard は一切変えない。impl 側に fixer ループを足さない。severity(序数)は
  導入しない。auto_merger / spec_fixer / ci_fixer のロジックは変えない。

## key decisions(A/B を全部ここで決める)

1. **verdict の形 — 三値 enum を採る(findings + flag ではなく)。**
   `ReviewVerdict = Clean | Advisory | Blocking`。序数 severity は入れない(ADR 0022)。
2. **schema は kind 共通、意味は kind でパラメタ化する。** `ReviewFile` は一つ。verdict → status /
   カテゴリ検証 / プロンプトを kind(Plan/Impl)で分岐する。
3. **blocking カテゴリの正準値は `security | data-loss | cost | performance` の閉じた列挙。**
   `blocking_categories: Vec<BlockingCategory>` で表す。**impl の blocking のみ非空・閉列挙を必須**とし、
   Plan の blocking と clean/advisory は空を許す。
4. **Plan の verdict 語彙は `clean | blocking` のみ(advisory を提示しない)。** Plan の settle は
   非 clean をすべて従来の findings 扱い(failure・spec-reviewing 維持・spec_fixer に委譲)にする。
   Plan の blocking はカテゴリを要求しない(品質ゲートのまま)。防御的に、万一 Plan が advisory を
   書いても非 clean パスに畳む。
5. **gate は commit status を持ち替えるだけ。** verdict → status を settle で対応させる:
   Impl `clean|advisory → Success` / `blocking → Failure`、Plan `clean → Success` / `非 clean → Failure`。
   auto_merger の pr_review gate は変更しない(Success→進行 / Failure→escalate / Pending→wait のまま)。
6. **後方互換のため `Blocking` に `#[serde(alias = "findings")]` を付ける。** 実行中 run の
   checkpoint や旧語彙で書く agent の `"findings"` を `Blocking` として読む(migration 参照)。
7. **impl blocking → needs-human 直行は現状維持。** fixer ループは足さない。
8. **advisory は event と `<details>` に残すのみ。** impl advisory を fix ループに繋がない。

## verdict × kind の挙動表(settle)

| kind | clean | advisory | blocking |
|------|-------|----------|----------|
| Impl | Success(進行) | **Success(進行・`<details>` に記録)** | Failure → needs-human(現状維持) |
| Plan | Success + spec-ready | 非 clean 扱い(下記) | Failure・spec-reviewing 維持・spec_fixer へ委譲 |

Plan の「非 clean」= 今の findings パス(byte-for-byte 維持):failure status / spec-reviewing 維持 /
`pr_review.deferred_to_spec_fixer` emit / needs-human は付けない。

## `.meguri/review.json` schema(agent 契約)

```json
{
  "verdict": "clean" | "advisory" | "blocking",
  "review": "<Markdown>",
  "blocking_categories": ["security" | "data-loss" | "cost" | "performance", ...]
}
```

検証(`read_review`、kind を渡す):

- `clean`: `review` 空でも可(nitpick は review に流す)。`blocking_categories` は空でなければ拒否。
- `advisory`: `review` 非空を要求。`blocking_categories` は空。
- `blocking`:
  - `review` 非空を要求。
  - **kind == Impl** のとき `blocking_categories` 非空かつ全要素が閉列挙に属することを要求。
  - **kind == Plan** のとき `blocking_categories` は要求しない(空でよい)。
- チェックアウト不改変(tree clean・HEAD 固定)の既存検証はそのまま。

## プロンプト

- **Impl(書き換え)**: 「self-review が品質を収束させ済み。あなたは安全 tripwire。次の4カテゴリを
  **明示できるときだけ** blocking:security / data-loss / cost / performance。カテゴリを名指しできない、
  あるいは疑わしいだけなら advisory。advisory は止めず記録されるだけ。何も無ければ clean」。
- **Plan(ほぼ現行維持)**: 「correctness, completeness, fit with conventions を見る。何も直す必要が
  無ければ clean(nitpick は review に書く・止めない)、直すべきものがあれば blocking」。advisory は
  提示しない。

## 触るファイル

- `src/engine/pr_reviewer.rs`
  - `ReviewVerdict` を三値化(`#[serde(alias="findings")]` on `Blocking`)、`BlockingCategory` 追加。
  - `ReviewFile` に `blocking_categories` 追加。`read_review` を kind 付き検証に。
  - `execute_prompt` を kind で impl/plan に分岐(上記プロンプト)。
  - `settle`:verdict × kind の status/ラベル分岐(挙動表)。advisory は Success + `<details>` 記録。
  - `pr_review_details` の outcome 文字列に advisory / blocking(カテゴリ併記)を追加。
  - `PrReviewCheckpoint` の `verdict`/新フィールドの持ち回り。
- `src/config.rs` … 変更なし想定(guard トグルは既存の `[review.guard]` を流用)。
- `src/engine/auto_merger.rs` … **変更なし**(status ベース gate のまま)。
- `src/engine/spec_fixer.rs` … **変更なし**(Plan の failure status 駆動のまま)。
- `docs/adr/0025-*.md` … 本 spec と同 PR で追加済み。

## observability

- `pr_review.posted` event に `verdict`(clean/advisory/blocking)と `categories` を載せる。
- impl blocking は従来どおり `pr_review.escalated` を emit。
- これで guard verdict 分布と blocking 率(狙い:数%台)が events で追える。

## migration & rollback

- **持続 DB schema 変更なし。** `.meguri/review.json` は実行時に毎回生成される制御ファイルで、
  デフォルトブランチにもコミットしない。
- **実行中 run の checkpoint 互換。** `PrReviewCheckpoint.verdict` は run step の永続 JSON に
  `"findings"` として載りうる。`Blocking` に `#[serde(alias="findings")]` を付けることで、デプロイ
  直後に resume した in-flight run も旧値を `Blocking` として読める(deser 失敗→`unwrap_or_default`
  でのリセットを避ける)。
- **rollback**: 挙動変更のみなので revert で戻る。戻した後に旧バイナリが読む checkpoint には
  新語彙 `"advisory"`/`"blocking"` が載りうるが、旧 `ReviewVerdict` は `#[serde]` で未知値を拒否する。
  そのため rollback 時は「in-flight run が settle 前で止まっていれば再 review される」ことを許容する
  (park はされず次掃引で再取得)。運用上は掃引間隔内で収束するため受容する。

## test strategy(FakeForge / FakeMux で record ベース検証)

- `read_review`: advisory は review 非空必須 / blocking(impl)はカテゴリ非空・閉列挙必須 /
  blocking(plan)はカテゴリ不要 / 未知カテゴリは拒否、を単体で。
- `settle`(impl): advisory → `Success` status・needs-human 無し・`<details>` に advisory 記録・
  auto-merge を止めないこと。blocking → `Failure`・needs-human(既存テスト維持)。
- `settle`(plan): clean → spec-ready(既存)/ 非 clean → failure・spec-reviewing 維持・
  needs-human 無し(既存テスト維持)。
- `execute_prompt`: impl はカテゴリ列挙と「疑わしいだけなら advisory」を含む / plan は従来文言を保つ。
- 統合(`tests/*.rs`):impl advisory の PR が auto-merge まで通り、impl blocking の PR が
  needs-human に park することを、pseudo-agent TUI で通しで確認する。

## 受け入れ観点(issue 由来)

- advisory findings の impl PR が needs-human にならず auto-merge まで通る。
- blocking findings の impl PR が従来どおり needs-human に park する。
- guard verdict 分布が events で観測でき、blocking 率の推移を追える。
- Plan の挙動は不変(既存テスト green)。
