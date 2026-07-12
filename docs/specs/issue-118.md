# issue-118 spec — リリース自動化: タグ駆動で GitHub Release(バイナリ)+ CHANGELOG + crates.io publish

現在タグが 1 つもなく、インストール手段は `cargo install --path .` のみ。公開後の安定運用のため、`vX.Y.Z` タグの push を起点に GitHub Release(バイナリ添付)・CHANGELOG・crates.io publish を自動化する。

この spec の中心的な決定(司令塔ツールを何にするか)は spec より長生きするので **ADR 0007**(本 PR 同梱)に置いた。ここでは何を作るか・触るファイル・受け入れ基準に絞る。

## 決定の要旨(詳細は ADR 0007)

- 司令塔は **自前のタグ駆動ワークフロー** `.github/workflows/release.yml`。**release-plz も cargo-dist も採用しない。**
  - release-plz を退ける理由: 中核価値の自動 version bump は Conventional Commits 前提で、本リポジトリは採用していない(`renovate.json5` に明記)。
  - cargo-dist を退ける理由: release.yml を生成・再生成し、「手書き + SHA ピン + zizmor クリーン」という CI 衛生の不変条件と衝突する。
- CHANGELOG エンジンの **git-cliff だけ** を単体で採用する(`cliff.toml` + `CHANGELOG.md`)。
- crates.io に **publish する。** OIDC Trusted Publishing で CI から。長期 secret を持たない。

## 作るもの

### 1. `.github/workflows/release.yml`(新規)— タグ駆動リリース

`on: push: tags: ['v*']`。ci.yml と同じ衛生を全ジョブに適用する: 全 action を SHA 固定 + バージョンコメント、harden-runner(egress: audit)、`actions/checkout` は `persist-credentials: false`、permissions はジョブ単位で最小、`timeout-minutes` 明示。

ジョブ構成(疎結合な 3 ジョブ、publish は分離して段階投入・キル可能にする):

1. **`build`** — バイナリを 2 ターゲットでビルドしてアーティファクト化。マトリクス:
   - `aarch64-apple-darwin`(runner: macOS arm64。`macos-14` 系)
   - `x86_64-unknown-linux-gnu`(runner: `ubuntu-latest`)
   - 各ターゲットで `cargo build --release --locked`、成果物を `meguri-<tag>-<target>.tar.gz` に固め、`.sha256` を併置。
   - `permissions: contents: read`。
   - 補足: rusqlite は `bundled`(SQLite を同梱ビルド)なので、Linux は将来 `x86_64-unknown-linux-musl` の静的バイナリに寄せる余地がある。今回は要件どおり gnu を基準にし、musl はスコープ外(将来の改良として ADR/README には広げない)。
2. **`release`** — GitHub Release を作成しアセットを添付。
   - git-cliff で当該タグ分のリリースノートを生成 → Release 本文に。
   - `build` のアーティファクト(tar.gz + sha256)を `gh release upload` で添付。
   - `permissions: contents: write`(タグは既に存在。Release 作成とアセット添付のみ)。`needs: build`。
3. **`publish-crate`** — crates.io に publish(分離ジョブ)。
   - OIDC Trusted Publishing: `id-token: write` + `contents: read`。crates.io 公式の OIDC トークン交換 action で short-lived token を得て `cargo publish --locked`。
   - `needs: build`(ビルドが通ってから publish)。crates.io 側の Trusted Publisher 登録が未了の間は、この 1 ジョブだけを外して他を回せる形にしておく。

zizmor は `.github/workflows/` を丸ごと検査するため、この新ファイルは追加設定なしで既存 zizmor ジョブの検査対象に入る(#118 やること 6 番目を構造的に充足)。Renovate の `helpers:pinGitHubActionDigests` / `pinDigests: true` も全 action を自動追従するので、新 action のピン更新は既存機構に乗る。

### 2. `cliff.toml`(新規)+ `CHANGELOG.md`(新規、永続ファイル)

git-cliff の設定。Conventional Commits ではないため、コミット subject を素直に列挙する形にパースルールを緩める(セクション分けを強制しない / マージコミットや `Co-Authored-By` を除外)。`CHANGELOG.md` はリポジトリルートの永続ファイルとして seed し、以降は git-cliff で更新する。**spec と違い CHANGELOG.md は使い捨てではない**(ADR 0001 の対象外)。

### 3. README(`README.md` / `README.ja.md`)2 枚

- **Status / ロードマップ節に SemVer 運用方針を一言**: 現在 `0.x`(pre-1.0)で、public API は未安定、0.x では minor でも破壊的変更があり得ること、`1.0.0` 到達までは patch/minor で追随する旨。#118 やること 4 番目。
- **Install 節**: `cargo install --path .` に加えて、(a) GitHub Release からのバイナリ取得、(b) crates.io publish 後の `cargo install meguri` を追記。ランタイム依存(git / gh / tmux または herdr)は既存の prereqs 記述のままで、バイナリ配布でも同じ前提であることを明記。
- リリース手順(version bump → CHANGELOG → `v*` タグ push)を短く。タグ駆動ゆえ誤タグが誤リリースになる点への注意も一言。

## 触るファイル

- `.github/workflows/release.yml` — 新規(タグ駆動、3 ジョブ、ci.yml と同衛生)
- `cliff.toml` — 新規(git-cliff 設定)
- `CHANGELOG.md` — 新規(永続。git-cliff で seed / 更新)
- `README.md` / `README.ja.md` — SemVer 方針・インストール手段・リリース手順
- `docs/adr/0007-tag-driven-self-owned-release-workflow.md` — 決定の記録(本 PR に同梱済み)
- `Cargo.toml` — 変更なし想定(publish メタデータは #113 で整備済み。crates.io publish で追加要件が出た場合のみ最小限)

## 受け入れ基準(acceptance criteria)

1. `v*` タグ push で `release.yml` が発火し、`aarch64-apple-darwin` と `x86_64-unknown-linux-gnu` のバイナリ(tar.gz + sha256)がビルドされる。
2. 同 run で GitHub Release が作成され、上記アセットが添付され、本文に git-cliff 生成のリリースノートが載る。
3. `publish-crate` ジョブが OIDC Trusted Publishing(long-term secret なし、`id-token: write`)で crates.io publish を行う構成になっている。crates.io 側設定が未了でも他ジョブを回せるよう分離されている。
4. `release.yml` の全 action が SHA 固定 + バージョンコメント、permissions はジョブ単位で最小、harden-runner + `persist-credentials: false` が入っている。**zizmor ジョブ(既存)が新ファイルを検査して指摘 0**。
5. README(en/ja)の Status 節に SemVer(0.x)方針が一文あり、Install 節にバイナリ取得と `cargo install meguri`(publish 後)が追記されている。
6. `CHANGELOG.md` と `cliff.toml` が追加され、git-cliff で CHANGELOG を生成できる。
7. `cargo build --release --locked` が両ターゲット相当で通る(CI 上で担保)。既存 CI(fmt / clippy / nextest / cargo-deny / zizmor)は非破壊。

## テスト / 検証計画

- ワークフローは実タグを切らずに検証する: `act` もしくは feature ブランチ上の一時的な軽量タグ(後で削除)で `build` ジョブのマトリクスが両ターゲットで走ること、アーティファクトが所定名で生成されることを確認。crates.io publish は `--dry-run` で疎通のみ確認し、実 publish はメンテナが crates.io 側の Trusted Publisher 登録を済ませてから初回タグで行う。
- git-cliff はローカルで `git cliff` を実行し、非 Conventional な履歴でも読める CHANGELOG が出ることを確認。
- zizmor はローカル(`zizmor .github/workflows/release.yml`)と CI の既存ジョブの両方で指摘 0 を確認。

## 運用上の前提(人手 / スコープ外)

- **crates.io の Trusted Publisher 登録**(GitHub リポジトリ + workflow を信頼発行元として登録)と **crate 名 `meguri` の空き確認** は publish の前提で、人手の初期設定。実装ブロッカーではないが、初回 publish 前に必要。
- Windows / macOS x86_64 / Linux aarch64 など追加ターゲット、Homebrew tap、静的 musl バイナリ、署名 / notarization は将来の改良でスコープ外。
- Conventional Commits の採用と、それに伴う release-plz の司令塔昇格は別判断(ADR 0007 が余地を残している)。本 spec では扱わない。
