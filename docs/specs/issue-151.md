# issue #151: 獲得スキルの apm パッケージ化 + 配布チャネル

エージェント向け skills シリーズ (3/3)。#147 で書いた獲得(acquisition)用スキル
`skills/meguri/SKILL.md` を、meguri **未導入**のユーザーが `apm install` 一発でユーザーレベルに
入れられる配布路として整える。設計判断は ADR 0012 に確定済み。本 spec は実装の受け入れ条件を書く。

## スペックの深さ: normal(理由)

最大の不確実性だった「1リポジトリ内サブパスでパッケージを切れるか」は、apm 0.24.0 の実機検証で
解消済み(下記)。残りは README とドキュメント追記、最小 CI ジョブ 1 本で、永続状態・スキーマ・
バイナリの公開契約には触れない。ブラスト半径は「ユーザーが叩く install コマンド」と CI 1 ジョブに
限られる。よって normal spec で足りる(migration/rollback セクション不要 — 永続状態を触らない)。

## 検証で確定した事実(実装はこれを前提にする)

apm 0.24.0 で確認済み:

- `skills/meguri/` は `SKILL.md` を含むので、**専用の `apm.yml` なしに** apm が Claude skill
  パッケージとして自動検出する。
- GitHub サブパス参照が通る: `apm install kkato1030/meguri/skills/meguri` →
  `github.com/kkato1030/meguri/skills/meguri @<sha>` を解決し(`--target claude` 時)
  `.claude/skills/meguri/` へ展開。ロックには `virtual_path: skills/meguri` が記録される。
  ルートの `apm.yml`(meguri 開発用)とは干渉しない。
- ターゲット別展開: `claude` → `.claude/skills/`、`codex` / `copilot` / `agent-skills` → 共有の
  `.agents/skills/`。`SKILL.md` は変換されず素通しで配られる(二重管理なし)。
- **ユーザースコープのデフォルトターゲットは Claude ではない**: クリーンな `HOME` でターゲット
  未指定の `apm install -g` を実行すると、共有の `~/.agents/skills/` にのみ展開され
  `~/.claude/skills/` には入らない。Claude Code で確実に発火させるには `--target claude` の
  明示が必要(ADR 0012)。よって README の配布コマンドは必ず `--target claude` を含める。
- unpinned 参照には apm が「`#tag`/`#sha` を付けろ」と警告する → 配布例は必ずタグにピンする。

## やること(受け入れ条件)

### 1. README に「エージェントに meguri を知らせる」節を追加
`## Install & set up` 配下、既存の `### Agent instructions (apm)`(L457 付近)とは別項として、
配布の 2 チャネルを書く:

- [ ] **未導入者向け(獲得)**: `apm install -g --target claude kkato1030/meguri/skills/meguri#<tag>`
      を提示。これで meguri 未導入のリポジトリでもユーザーレベルでスキルが発火する、と 1〜2 行で
      説明。`--target claude` は省略不可(省略すると `~/.agents/skills/` に入り Claude Code で
      発火しない — 上記検証)。タグは最新リリース(ADR 0007 の `vX.Y.Z`)にピンする旨を添える
      (unpinned 警告の回避)。
- [ ] **導入済み向け(定着)**: `meguri agent-skills install`(#150)を提示。
      → #150 未マージなら「(#150 で提供予定)」と明記し、コマンド行だけ先に置くかは実装判断。
- [ ] 日本語 README(`README.ja.md` があれば)にも同じ 2 チャネルを反映。

### 2. 最小 CI ドリフトチェック(ADR 0012 §4)
- [ ] `ci.yml` に、`skills/meguri/` が各ターゲット(claude / codex / copilot / agent-skills)へ
      インストール/コンパイルできることを確認する軽量ジョブを 1 本足す。スクラッチディレクトリへの
      install(`--root <dir>` か使い捨て cwd)が成功し、`SKILL.md` が展開されることを assert する
      程度でよい(症状: apm のバージョン更新で獲得チャネルが黙って壊れるのを検知する)。
      `claude` ターゲットは展開先が `.claude/skills/meguri/SKILL.md` であることまで assert する
      (apm のターゲット別配置が将来変わったときも README の配布コマンドの約束が守られているかを
      ここで検知する)。
- [ ] **要判断(実装時)**: CI での apm 導入方法とピン。ADR 0007 のサプライチェーン衛生
      (SHA ピン / harden-runner / 最小 permissions)を保つこと。apm バージョンは `apm.lock.yaml`
      の `apm_version`(現在 0.24.0)と揃える。curl 一発インストールは衛生に反するので、
      チェックサム検証付きバイナリ取得か mise 等の固定を検討する。

### 3. 将来候補の issue 化(本 issue のスコープ外)
- [ ] Claude Code plugin marketplace への登録を「将来候補」として follow-up issue に切り出す。
      本 spec では**内容だけ**を下記に控えておき、実際の起票はメンテナ/後続に委ねる(この turn は
      spec 作成のみ。issue は作らない):
      - タイトル案: 「獲得スキルを Claude Code plugin marketplace に登録する(将来候補)」
      - 本文骨子: ADR 0012 は GitHub 参照配布に留めた。marketplace 登録は発見性を上げるが、
        pre-1.0 では過剰。1.0 前後で再検討。`apm pack --target claude` で plugin.json を生成できる
        ことは確認済み。

## 触るファイル

- `README.md`(と `README.ja.md` があれば) — Install 節に 2 チャネルの項を追加。
- `.github/workflows/ci.yml` — 最小ドリフトチェックジョブを追加。
- `docs/adr/0012-acquisition-skill-as-apm-subpath-github-ref.md` — 本 spec と同時に追加済み(判断の正)。
- `skills/meguri/`(既存)— **変更しない**。ソースの正はここ(ADR 0009)。apm 用の別ファイルは作らない。

## やらないこと(スコープ外)

- 別リポジトリ `kkato1030/meguri-skills` の作成(サブパスで成立するため不要 — ADR 0012)。
- apm レジストリへの `apm publish`(レジストリ基盤を持たない — ADR 0012)。
- `release.yml` への publish ジョブ追加(タグを打てば install 可能になるため不要 — ADR 0012)。
- `SKILL.md` 本文の apm 用変換版の作成(素通しで配れる — ADR 0009/0012)。
- Claude Code plugin marketplace への実際の登録(将来候補として issue 化のみ)。

## 判断ログ(なぜこの深さ・この配布路か)

サブパス成立の実機検証で最大の不確実性が消えたため normal spec。配布は別リポジトリ・レジストリの
どちらも避け、本体サブパスの GitHub 参照 install に一本化した(ソース二重管理ゼロ・publish 運用コスト
ゼロ)。詳細な理由は ADR 0012 を参照。
