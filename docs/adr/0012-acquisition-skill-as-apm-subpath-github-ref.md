# ADR 0012: 獲得スキルは同一リポジトリのサブパス apm パッケージとして GitHub 参照で配布する

- Status: accepted
- Date: 2026-07-14
- Issue: #151

## Context

ADR 0009(#147)で書いた獲得(acquisition)用スキル `skills/meguri/SKILL.md` は、meguri が
**まだ入っていない**リポジトリで発火してこそ意味がある。つまりユーザーレベル(ユーザーの
`~` 配下)に事前インストールされている必要がある。プロジェクトレベルには置けない。この
「ユーザーレベルに一発で入れる」配布路をどう作るかが本 ADR の主題である。

論点が3つあった。

1. **1リポジトリでパッケージを2つ切れるか。** リポジトリルートの `apm.yml` は meguri 自身の
   開発用指示のソース(ADR 0008 / #137)であり、既に「meguri」という別パッケージとして使って
   いる。獲得スキルを同じリポジトリから別パッケージとして配れないなら、`kkato1030/meguri-skills`
   のような別リポジトリに分離するしかなく、その場合ソースの二重管理(本体 `skills/` と別
   リポジトリ)が発生する。

2. **配布の器をどうするか。** apm には `apm publish`(レジストリへのアップロード)と、
   GitHub 参照からの直接 install の2系統がある。レジストリは experimental フラグ
   (`apm experimental enable registries`)とホスト先のレジストリ実体が要る。

3. **ソースの二重管理を作らないこと。** ADR 0009 は `SKILL.md`(Claude 形式)を正と定めた。
   apm 用に変換した第二の本文を持つと、直すたびに両方を追う羽目になる。

## Decision

**獲得スキルは、本体リポジトリの `skills/meguri/` サブパスをそのまま apm パッケージとして
GitHub 参照でインストールさせる。別リポジトリもレジストリも作らない。**

配布路(未導入ユーザー向け):

```bash
apm install -g --target claude kkato1030/meguri/skills/meguri#<tag>
```

`-g` でユーザースコープに入り、`--target claude` で Claude Code のユーザーレベルスキル
(`~/.claude/skills/meguri/`)として展開される。これで meguri 未導入のリポジトリでもスキルが
発火する。

**`--target claude` は省略できない。** apm 0.24.0 をクリーンな `HOME` で実機確認したところ、
ターゲット未指定の `apm install -g` は共有の `~/.agents/skills/` にのみ展開し
(`Skill integrated -> .agents/skills/`)、`~/.claude/skills/` には入らなかった。獲得チャネルの
約束(meguri 未導入のリポジトリでも Claude Code でスキルが発火する)を確実に満たすため、
配布例は必ずターゲットを明示する。`.agents/skills/` を正式経路に採る案は退けた — Claude Code の
ユーザーレベルスキルの置き場として文書化されているのは `~/.claude/skills/` であり、
`~/.agents/skills/` が Claude Code に読まれる保証を meguri 側で持てないため。

### 1. サブパスパッケージは実機で成立する(論点1の答え)

apm 0.24.0 で検証済み。`skills/meguri/` は `SKILL.md` を含むディレクトリなので、apm はこれを
**専用の `apm.yml` なしに** Claude skill パッケージとして自動検出する。GitHub 参照でも
サブパス指定 `owner/repo/subpath` がそのまま通り、ロックファイルに `virtual_path: skills/meguri`
として記録される(`--target claude` 時):

```
[+] github.com/kkato1030/meguri/skills/meguri @d0afd795
  |-- Skill integrated -> .claude/skills/
```

したがって**別リポジトリは不要**。ルートの `apm.yml`(meguri 開発用)とは干渉しない — サブパス
install はルート `apm.yml` を読まず、`skills/meguri/SKILL.md` だけを見る。ソースの正は ADR 0009
のまま本体 `skills/meguri/` に一本化される。

### 2. レジストリではなく GitHub 参照で配る(論点2の答え)

`apm publish` はレジストリ機能(experimental)とホスト先が前提で、pre-1.0・単独運用の meguri には
過大。GitHub 参照 install はレジストリ基盤ゼロで同じことができる。apm は unpinned な参照に対して
「`#tag` か `#sha` を付けてドリフトを防げ」と警告するので、配布例は必ずリリースタグ(ADR 0007 の
`vX.Y.Z`)にピンする。

**リリースフロー(ADR 0007)には publish ジョブを足さない。** レジストリが無い以上「publish」は
「タグを打てば、そのタグの `skills/meguri/` サブパスがそのまま install 可能になる」ことと同義で、
新しい CI ステップは要らない。README がタグ付き install 例を提示するだけでよい。

### 3. 変換を作らず、SKILL.md を直接配る(論点3の答え)

apm は `SKILL.md` を変換せずそのまま各ターゲットへ展開する。Claude は `.claude/skills/`、
Codex / Copilot は共有の `.agents/skills/` に入る(実機確認済み)。ADR 0009 の「SKILL.md を正と
する」方針をそのまま満たし、apm 用の第二本文は存在しない。

### 4. ドリフト検知は最小の CI チェックで担保する

apm は 0.x で動きが速い(ADR 0008 と同じ懸念)。apm のバージョンが上がって獲得チャネルが黙って
壊れることを防ぐため、`skills/meguri/` が各ターゲットへ問題なくインストール/コンパイルできることを
確認する最小ジョブを CI に足す。詳細な実装(apm のピン方法・ジョブ構成)は spec 側で詰める。

## Consequences

- 配布物のソースは本体リポジトリ 1 箇所(`skills/meguri/`)に一本化され、別リポジトリ同期の
  二重管理は発生しない。ADR 0009 の「SKILL.md が正」がそのまま配布層まで貫通する。
- ユーザーは `apm install -g --target claude kkato1030/meguri/skills/meguri#<tag>` の 1 コマンドで獲得スキルを
  入れられる。導入済みリポジトリ向けの定着経路(`meguri agent-skills install`、#150)とは
  別チャネルとして共存する。
- レジストリを持たないので publish 運用コストはゼロ。反面、apm レジストリのエコシステム
  (検索・バージョン一覧)には載らない。pre-1.0 では GitHub 参照で十分という判断であり、
  将来レジストリや Claude Code plugin marketplace に載せる余地は閉じない(marketplace 登録は
  別 issue に切り出す)。
- CI に apm への依存(外部ツール・ネットワーク)が最小ジョブとして 1 つ増える。ADR 0008 が
  apm を本体フローから外していたのに対し、ここでは「配布物が壊れていないか」の検知目的で
  限定的に CI へ持ち込む。apm 本体のピン(サプライチェーン衛生、ADR 0007)を保つ必要がある。
