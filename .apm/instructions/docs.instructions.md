---
description: ADR と spec の運用、ラベルモデルへの参照
applyTo: "docs/**"
---

- `docs/adr/NNNN-slug.md` は恒久的な設計判断の記録である。一度書いたら削除・改訂ではなく、
  判断が変わったら新しい ADR を積む。番号は次の空き番号を使う。刈り込みで休眠した機構の
  ADR は `docs/adr/STATUS.md` の台帳で dormant として管理する（ファイル自体は残す）。
- 長期的に価値がある内容（設計判断の理由・ドメイン規則）は ADR か既存の永続ドメイン文書へ
  振り分ける。
- issue ラベルは権威反転後のモデルに従う: `ready`/`hold` は人間のエッジ入力（intake が読む）、
  `working`/`implementing`/`needs-human` は sqlite の task 状態の best-effort 投影。
