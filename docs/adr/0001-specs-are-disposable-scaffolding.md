# ADR-0001: spec は使い捨ての足場 — 実装時に刈り、永続知識は ADR / ドメイン文書へ

- Status: Accepted
- Date: 2026-07-11
- Issue: #48

## 文脈

meguri の spec 先行フローでは、planner が `docs/specs/issue-<N>.md` を書き、reviewer がそれをレビューし、spec-worker が同じブランチの上に実装を積む。spec の実体は「レビューを収束させるための足場」だ。planner の検証は存在確認だけであり、spec-worker は spec が読めなくても graceful に degrade する。つまりシステムの誰も、merge 後の spec に依存していない。

それなのに spec は実装 commit と一緒にデフォルトブランチへ merge され、永久に残っていた。役目を終えた足場が撤去されないまま、`docs/specs/` には issue 番号順の陳腐化したスナップショットが単調に積もっていく。寿命（レビュー期間だけ）と保存形式（永久保存）がねじれていた。

一方で、spec には時おり長生きすべきものが紛れ込む。設計判断の理由や、システムが長期的に満たすべきドメイン規則だ。spec を刈るだけの施策では、それらが行き場を失って一緒に消えてしまう。

## 決定

寿命と保存形式を一致させる。

1. **spec は使い捨てに徹する。** spec-worker は実装完了時に `docs/specs/issue-<N>.md` を削除して commit し、spec をデフォルトブランチに merge させない。orchestrator 側の検証もこれに合わせる: planner は「spec が存在しなければ Err」、spec-worker は「spec がまだ存在すれば Err」— 対称な検証で寿命の両端を挟む。
2. **永続価値は書く時点で振り分ける。** planner のプロンプトは、spec が実装時に消えることを明示した上で、設計判断（なぜその approach か）は ADR（`docs/adr/NNNN-<slug>.md`、次の空き番号）へ、長期的なビジネスロジック / ドメイン規則はリポジトリ既存の永続ドメイン文書へ（無ければ、その issue がそうした規則を導入する場合に限り新設して）書くよう指示する。
3. **`issue-<N>` という命名は維持する。** spec のパスは issue 番号だけから機械的に再構成できるキーであり、planner（書く）・spec-worker（読む）・検証の3箇所がこれに依存する。使い捨ての足場に content-addressable な名前が付いているのは、役割に対して正しい。

## 帰結

- `docs/specs/` はデフォルトブランチ上では常に空（またはレビュー中の spec だけが in-flight ブランチに存在する）。墓場は増えない。
- merge 済み PR の diff からは spec が消える（planner の add と spec-worker の delete が相殺する)。spec の内容を後から参照したければ PR の履歴か、そこから振り分けられた ADR / ドメイン文書を見る。これは意図した挙動である。
- 設計判断の永続記録はこの `docs/adr/` に積もる。本 ADR がその最初の一枚であり、同時に新方式の最初の実例でもある。
- spec-worker の corrective ターンが「spec の削除し忘れ」を拾うようになる。エージェントが削除を忘れても、1 回の corrective ターンで回収される。
