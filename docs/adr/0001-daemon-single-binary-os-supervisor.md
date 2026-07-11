# ADR 0001: daemon は単一バイナリ、監視は OS(launchd)に委ね、CLI↔daemon IPC を持たない

- Status: accepted
- Date: 2026-07-11
- Issue: #25

## Context

`meguri watch` を常駐化するにあたり、先行実装の looper は `looper`(CLI)と
`looperd`(daemon)をバイナリ分離し、localhost HTTP(`/api/v1/*`)+ 契約テストで
CLI↔daemon の互換性を担保している。meguri で同じ重装備を最初から持つかが論点だった。

meguri には looper と異なる前提が 2 つある:

1. **すでに kill-safe。** Authority は forge のラベル/コメント、run の進行は sqlite
   checkpoint にあり、watch 再起動時の recovery が running run を再アダプト/再開する。
   daemon 化の価値は「復旧できる」ではなく「勝手に起動し続ける + 状態が見える」に絞られる。
2. **状態はすべて共有ストレージにある。** run の一覧・進行・イベントは sqlite、
   supervision メタデータは小さなファイルで表現できる。CLI が daemon プロセスに
   問い合わせないと得られない情報が(現時点では)存在しない。

## Decision

1. **単一バイナリ。** `megurid` は作らない。daemon の実体は同一バイナリの
   `meguri watch` であり、`meguri daemon start` が detach して spawn するか、
   launchd が直接起動する。バージョンスキュー問題(CLI と daemon の別バージョン共存)は
   構造的に発生しない。
2. **プロセス監視は OS に委ねる。** 自前 supervisor を書かず、macOS では user
   LaunchAgent の `KeepAlive` / `ThrottleInterval` に restart policy をマップする。
   detached モード(pidfile のみ、クラッシュで復活しない)は軽量な入口として残す。
   非対応プラットフォームは明示エラー(silent fallback しない)。Linux は systemd
   user unit を同じ設計で後続追加する。
3. **CLI↔daemon IPC を持たない。** `daemon status` 等は
   state ファイル(`~/.meguri/daemon/state.json`)+ `kill(pid, 0)` +
   `launchctl print` + sqlite 直読みで答える。単一インスタンス保証は pidfile の
   liveness 推測ではなく、watch プロセスが保持する排他 flock で行う。

## Consequences

- 配布・upgrade が単純(1 バイナリ)。looper の `upgrade --cli/--daemon` 分離や
  HTTP 契約テストは不要。
- 「走行中 daemon への問い合わせ」(attach 中の live 状態、graceful drain 指示など)が
  必要になった時点で IPC を導入する(Phase 3)。その際も sqlite/state ファイルという
  fallback 経路が残るため、IPC は追加であって置き換えにならない。
- launchd に委ねる代償として、restart policy の変更は plist 再生成
  (`meguri daemon install` 再実行)が必要。プロセス内 hot-reload はできない。
- watch プロセスは login セッション(Aqua)内で動く前提を持つ。herdr/tmux の
  unix socket・`gh` keychain へ届くのはこの前提による。セッション外で動かす要件
  (ヘッドレスサーバー等)が出たら、mux server を daemon 自身が起動する方式を再検討する。
