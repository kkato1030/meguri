# ADR 0007: リリースは自前のタグ駆動ワークフローで回す — release-plz も cargo-dist もオーケストレータには据えない

- Status: accepted
- Date: 2026-07-12
- Issue: #118

## Context

公開後の安定運用に向けて、タグから GitHub Release(バイナリ添付)+ CHANGELOG を自動化したい。現在タグは 1 つもなく、インストール手段は `cargo install --path .` のみ。issue #118 は候補ツールを二つ挙げている: **release-plz**(release PR 駆動、crates.io と相性が良い)と **cargo-dist**(マルチプラットフォームバイナリ配布に強い)。

論点は「リリースの司令塔を何にするか」だ。この選択は二つの既存の前提と衝突する。

1. **このリポジトリの CI は "自前で所有し、SHA でピンし、zizmor で検査する" もの。** `.github/workflows/ci.yml` は手書きで、全 action が SHA 固定 + バージョンコメント付き、harden-runner の egress 監査、最小 permissions、zizmor ゲートが揃っている(issue #77 / #78)。リリースワークフローにも同じ衛生を適用することが #118 の受け入れ条件そのもの(やること 6 番目)。

2. **コミット subject が「変更の内容」ではなく「issue の意図」に由来している。** 本リポジトリは squash マージで main に載る commit subject = PR タイトルだが、その PR タイトルはエンジンが機械的に `"{issue_title} (#N)"` を組み立てたもの(各ループの `pr_title`、`src/engine/*.rs`)であり、エージェントが author しているのは本文(`summary` / `pr_body`)だけ。結果として、変更の性質(feat/fix/…)を表す情報はコミットにも PR にもどこにも存在せず、`renovate.json5` も「コミット履歴は conventional commits ではないため semanticCommits はデフォルトのまま」と明記している。この帰結として、Conventional Commits の採用は設定トグルではなく **エンジン改修(型分類の新設)** を要する — 詳細は下記の release-plz の項。

## Decision

**リリースは自前のタグ駆動ワークフロー(`.github/workflows/release.yml`)で回す。オーケストレータとして release-plz も cargo-dist も採用しない。両者が使う CHANGELOG エンジンである git-cliff だけを単体で採り入れる。**

司令塔は「`vX.Y.Z` タグの push」という一点に固定する。メンテナが version bump と CHANGELOG 更新を通常の PR で行い、`v*` タグを push すると `release.yml` が発火して、(a) 2 ターゲットのバイナリをビルドして GitHub Release に添付し、(b) git-cliff でリリースノートを生成し、(c) crates.io に publish する。crates.io は **初回だけメンテナが手動 `cargo publish` で crate を確保** し、その後 Trusted Publisher を登録して **2 回目以降を OIDC Trusted Publishing** に載せる(crates.io は PyPI の pending publisher 相当を持たず、Trusted Publisher 設定は crate が一度 publish 済みでないと作れないため。下記 Consequences)。

### release-plz を司令塔に据えない理由

release-plz の中核価値は「Conventional Commits を読んで version を自動 bump し、release PR を開く」ことにある。この前提が本リポジトリには無い。しかも(2 の観点で見たとおり)それは「規約を守っていない」だけの問題ではなく、**変更の型分類がそもそもエンジンのどこにも生成されていない** という構造の問題だ。squash マージ下で release-plz が読むのは PR タイトル由来の subject 1 本だが、そのタイトルは `pr_title` が issue タイトルから機械生成したもので、feat/fix の別を含まない。

したがって release-plz を活かすには、まず Conventional Commits を「採用」しなければならず、その採用の実体は設定変更ではなく:

- ターン完了契約(`src/turn/prompts.rs`)に変更型フィールドを新設し、
- 6 箇所の `pr_title` に型接頭辞を通し、人間 issue / planner の decompose で作られる issue タイトルも規約化し、
- PR タイトル lint の CI(新規 workflow → zizmor 衛生も付随)で強制する、

というエンジン改修になる。加えて、自律エージェントが型を誤分類 → semver bump を誤る → リリースは自動なので誤バージョンが出荷される、という **新しい故障モード** を 0.x の低頻度・低対価の局面で抱え込む。この段階では割に合わない。

**ただし、この却下は永続ではない。** 別途 **「PR/コミット subject をエージェントが『実際の変更』から author する」** という決定を進めており(issue タイトル流用をやめ、完了契約に `subject` 行を追加する。別 issue)、これが入るとタイトルは変更由来になり、CHANGELOG も subject 由来のまともな行になる(下記 git-cliff の項が直接依存)。**その基盤の上では Conventional Commits は「型分類パイプライン」ではなく `subject` への接頭辞 1 つに縮む**ため、release-plz の司令塔昇格は現実的な再訪になる(下記 Consequences)。単一 action なので衛生面(1 の観点)では release-plz に致命的な問題は無い — 現時点で退けるのはもっぱら 2 の観点による。

### cargo-dist を司令塔に据えない理由

cargo-dist は release.yml を **生成し、再生成する**。action のピン方式・permissions・ジョブ構成を cargo-dist が所有するため、本リポジトリの「手書き + SHA ピン + zizmor クリーン」という不変条件(1 の観点)と正面から衝突する。生成物を毎回オーバーライドして衛生を保つのは、自前で書くより高コストで壊れやすい。マルチプラットフォームのインストーラ生成という強みは、macOS arm64 / Linux x86_64 の 2 ターゲットという現在の要件には過剰。

### git-cliff だけは採る

CHANGELOG 生成は release-plz も内部で git-cliff を使う。git-cliff は Conventional Commits でなくてもコミット subject を列挙する CHANGELOG を生成できる(`cliff.toml` でパースルールを緩められる)。司令塔ツールを入れずに、この部品だけを `cliff.toml` + `CHANGELOG.md` として単体で採用する。

ただし CHANGELOG の質は subject の質に等しい。現状の subject(= issue タイトル)には「OSS 公開 (6/7):」のような進行管理ノイズが乗り、意図と結果がズレる(spec だけ書いた PR が「リリース自動化」と載る等)。そのため本 spec の CHANGELOG は上記 **subject をエージェントが変更由来で author する決定(別 issue)に依存** する。その決定が入る前に seed する初回 CHANGELOG は既存履歴由来で不揃いになりうる点を許容し、以降を改善する。

## Consequences

- リリースワークフローは全面的に自前所有 = ci.yml と同じ衛生(SHA ピン、最小 permissions、harden-runner、`persist-credentials: false`)をそのまま適用でき、zizmor は `.github/workflows/` を丸ごと検査するので新ファイルは追加設定なしで検査対象に入る。#118 やること 6 番目を構造的に満たす。
- version bump は手動。0.x の低頻度リリースでは許容範囲だが、リリース頻度が上がったら release PR の自動化が恋しくなる。**その転換点は 2 段階**: まず「subject をエージェントが変更由来で author する」決定(別 issue)が基盤として入り、その上で Conventional Commits を(subject への接頭辞として安価に)採用した時点で、release-plz を司令塔に昇格させる余地が開く。本 ADR はそれを閉ざさない。
- crates.io publish の **初回だけは手動**。crates.io は PyPI の pending publisher 相当を持たず、Trusted Publisher 設定は crate が一度 publish 済みでないと作れない(2025-07 時点)。したがって初回は メンテナが手動 `cargo publish`(一時 token)で crate 名 `meguri` を確保 → crates.io で Trusted Publisher(GitHub リポジトリ + `release.yml`)を登録 → **2 回目以降のタグが OIDC Trusted Publishing で publish** される、という順になる。以降は short-lived token のみを扱い、長期 secret を CI に置かない(#113 のメタデータが前提)。この初回手動 publish + Trusted Publisher 登録 + crate 名の空き確認は **人手の初期設定**(実装ではなく運用の一手)。
- タグ駆動なので「タグを間違えて push した」事故がそのまま誤リリースになる。保護タグ/手順の明文化(README)で運用リスクを下げる。CHANGELOG とバイナリは後から差し替えできるが、crates.io publish は取り消せない(yank のみ)ため、publish ジョブは分離して段階投入できる形にする。
