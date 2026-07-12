# ADR 0007: リリースは自前のタグ駆動ワークフローで回す — release-plz も cargo-dist もオーケストレータには据えない

- Status: accepted
- Date: 2026-07-12
- Issue: #118

## Context

公開後の安定運用に向けて、タグから GitHub Release(バイナリ添付)+ CHANGELOG を自動化したい。現在タグは 1 つもなく、インストール手段は `cargo install --path .` のみ。issue #118 は候補ツールを二つ挙げている: **release-plz**(release PR 駆動、crates.io と相性が良い)と **cargo-dist**(マルチプラットフォームバイナリ配布に強い)。

論点は「リリースの司令塔を何にするか」だ。この選択は二つの既存の前提と衝突する。

1. **このリポジトリの CI は "自前で所有し、SHA でピンし、zizmor で検査する" もの。** `.github/workflows/ci.yml` は手書きで、全 action が SHA 固定 + バージョンコメント付き、harden-runner の egress 監査、最小 permissions、zizmor ゲートが揃っている(issue #77 / #78)。リリースワークフローにも同じ衛生を適用することが #118 の受け入れ条件そのもの(やること 6 番目)。

2. **このリポジトリは Conventional Commits を採用していない。** コミット履歴は日本語の PR タイトル起点(例: `OSS 公開 (2/7): …`)であり、`renovate.json5` も「コミット履歴は conventional commits ではないため semanticCommits はデフォルトのまま」と明記している。

## Decision

**リリースは自前のタグ駆動ワークフロー(`.github/workflows/release.yml`)で回す。オーケストレータとして release-plz も cargo-dist も採用しない。両者が使う CHANGELOG エンジンである git-cliff だけを単体で採り入れる。**

司令塔は「`vX.Y.Z` タグの push」という一点に固定する。メンテナが version bump と CHANGELOG 更新を通常の PR で行い、`v*` タグを push すると `release.yml` が発火して、(a) 2 ターゲットのバイナリをビルドして GitHub Release に添付し、(b) git-cliff でリリースノートを生成し、(c) crates.io に OIDC Trusted Publishing で publish する。

### release-plz を司令塔に据えない理由

release-plz の中核価値は「Conventional Commits を読んで version を自動 bump し、release PR を開く」ことにある。この前提が本リポジトリには無い。commit が慣習に従わない以上、release-plz が開く release PR は毎回 patch bump を提案し、メンテナが毎回それを手で上書きすることになる — 自動化の主目的が空回りする。残るのは「タグ・GitHub Release・crates.io publish のプランビング」だけで、それは自前ワークフローで直接・かつ SHA ピンを自分の管理下に置いて表現できる。**Conventional Commits を導入すれば release-plz の採用は再考に値する**(下記 Consequences)。単一 action なので衛生面(1 の観点)では release-plz に致命的な問題は無い — 退けるのはもっぱら 2 の観点による。

### cargo-dist を司令塔に据えない理由

cargo-dist は release.yml を **生成し、再生成する**。action のピン方式・permissions・ジョブ構成を cargo-dist が所有するため、本リポジトリの「手書き + SHA ピン + zizmor クリーン」という不変条件(1 の観点)と正面から衝突する。生成物を毎回オーバーライドして衛生を保つのは、自前で書くより高コストで壊れやすい。マルチプラットフォームのインストーラ生成という強みは、macOS arm64 / Linux x86_64 の 2 ターゲットという現在の要件には過剰。

### git-cliff だけは採る

CHANGELOG 生成は release-plz も内部で git-cliff を使う。git-cliff は Conventional Commits でなくてもコミット subject を列挙する CHANGELOG を生成できる(`cliff.toml` でパースルールを緩められる)。司令塔ツールを入れずに、この部品だけを `cliff.toml` + `CHANGELOG.md` として単体で採用する。

## Consequences

- リリースワークフローは全面的に自前所有 = ci.yml と同じ衛生(SHA ピン、最小 permissions、harden-runner、`persist-credentials: false`)をそのまま適用でき、zizmor は `.github/workflows/` を丸ごと検査するので新ファイルは追加設定なしで検査対象に入る。#118 やること 6 番目を構造的に満たす。
- version bump は手動。0.x の低頻度リリースでは許容範囲だが、リリース頻度が上がったら release PR の自動化が恋しくなる。**その転換点は「Conventional Commits を採用するか」の判断とセット**で、採用するなら release-plz を司令塔に昇格させる余地を残す(本 ADR はそれを閉ざさない)。
- crates.io publish は OIDC Trusted Publishing で CI から行う(#113 のメタデータが前提)。short-lived token のみを扱い、長期 secret を CI に置かない。crates.io 側での Trusted Publisher 登録・crate 名の空き確認という **人手の初期設定** が publish の前提として残る(実装ではなく運用の一手)。
- タグ駆動なので「タグを間違えて push した」事故がそのまま誤リリースになる。保護タグ/手順の明文化(README)で運用リスクを下げる。CHANGELOG とバイナリは後から差し替えできるが、crates.io publish は取り消せない(yank のみ)ため、publish ジョブは分離して段階投入できる形にする。
