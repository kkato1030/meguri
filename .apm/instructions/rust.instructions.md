---
description: Rust コードの書き方(v2)
applyTo: "src/**"
---

- エラーハンドリングは `anyhow::Result` + `.context("...")` / `bail!("...")` で
  統一する。ライブラリ独自のエラー型は導入しない。
- `unwrap()` / `expect()` はテストコード(`#[cfg(test)]`)以外では使わない。
- git 操作は `src/gitops.rs` に集約する。他のモジュールから `git` プロセスを
  直接起動しない。
- 依存 crate は最小に保つ。新しい crate は、それが解く問題が実際に現れた増分で
  足す(Cargo.toml の方針コメント参照)。
- 通読可能性が第一。抽象(trait 等)は 2 つ目の実装が実際に現れるまで導入しない。
