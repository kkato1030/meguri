# meguri

> **meguri is a delivery control plane that turns intent into an
> executable work graph and coordinates humans and AI agents toward the
> desired state.**

Intent を実行可能な Work Graph に変換し、人間と AI Agent による実行・判断を
通じて Desired State への到達を管理する Delivery Control Plane。
初期は local-first で始める(現時点の重心であって定義ではない — `docs/plan.md` §1)。

開発計画は `docs/plan.md` を参照。

---

このリポジトリは 2026-08-16 にリセットされ、ゼロから再出発した。
旧実装(v1: issue 駆動の autonomous loop / v2: 増分書き直し)の履歴と
ADR 群(失敗カタログ)は、リモートの archive ブランチに保存されている:
`archive/v1-main` / `archive/v2` / `archive/v2-preflight`。
