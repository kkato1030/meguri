---
description: ADR と spec の運用、ラベルモデルへの参照
applyTo: "docs/**"
---

- `docs/adr/NNNN-slug.md` は恒久的な設計判断の記録である。一度書いたら削除・改訂ではなく、
  判断が変わったら新しい ADR を積む。番号は次の空き番号を使う。
- `docs/specs/issue-<N>.md` は使い捨ての足場である。planner が spec-first フロー
  （`meguri:plan`）で作成し、reviewer のレビューを収束させたら spec-worker が実装完了時に
  削除する。デフォルトブランチ上には残らない
  （`docs/adr/0001-specs-are-disposable-scaffolding.md`）。
- spec に書いた内容のうち長期的に価値があるもの（設計判断の理由・ドメイン規則）は、消える前に
  ADR か既存の永続ドメイン文書へ振り分ける。
- issue ラベルは「フェーズ」（`plan` → `speccing` → `ready` → `implementing`）と
  「ボールの所在」（`working` / `needs-human` / `hold`）の2軸モデルであり、独自のラベル運用を
  文書に追加する前に `docs/adr/0005-issue-labels-two-axis-phase-and-ball.md` を参照する。
