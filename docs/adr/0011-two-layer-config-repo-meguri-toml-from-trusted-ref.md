# ADR 0011: 二層 config — プロジェクト内在の設定は repo ルート `meguri.toml` に宣言し、trusted ref(default branch)からのみ読む

- Status: proposed
- Date: 2026-07-14
- Issue: #165

## コンテキスト

per-project の設定が host 側 `~/.meguri/config.toml` の `[[projects]]` に増殖している
(`check_command` / `language` / `clean.ignore` / `pr` …)。計画中の #149(prompts)・
#146(schedules)・#139(worktree_setup)もいずれも「repo 相対パスや repo 内の運用規律を
host config から指す」形で、**プロジェクト内在の設定が repo の外に住んでいる**歪みが顕在化した。
同じ repo を別ホストで回すと設定を手で複製することになり、repo と一緒にバージョン管理もされない。

一方で、「設定を repo に置く」ことには固有の危険がある。meguri の完了契約は独立検証
(`check_command` を meguri 自身が回す)に立脚しており(README の Security 参照)、その検証内容を
run 中のエージェントが**自分のブランチで書き換えられる**なら、契約は自己改竄可能になる。

同時に #154(workspace)の「置き場所は repo 側か host 側か」という論点も未決だった。

## 決定

repo ルートに `meguri.toml` を置けるようにし、**プロジェクト内在の設定を repo 側で宣言できる**
ようにする。どのキーを repo に置けるかは、次の**境界原理**で二分する。

> **repo config に書けるのは、その repo 自身の run にしか影響しない「プロジェクト内在の事実」だけ。
> 他の repo を名指しする設定・ホストマシンや token に束縛される設定・信頼の宣言は host config のみ。**

この一つの原理が、個別に決めていた分類を演繹に変える。

### キーの分類

| 層 | キー | 理由 |
|---|---|---|
| host 専用 | `[[workspaces]]`、`id`、`repo_path`、`repo_slug`、`mode`、`deliver`、`worktree_root` | 他 repo の名指し / マシン・token 束縛 / 信頼の宣言(config への登録 = 信頼行為) |
| host 専用 | `mux`、`agent(s)`、`routing`、`drift`、`limits`、`scheduler`、`daemon`、`notifications`、`pr.auto_merge` | ホスト運用者のリソース・権限。auto_merge は host の `gh` token でマージを arm するため保守的に host 側 |
| host 専用(bootstrap) | `default_branch` | 「どの ref から repo config を読むか」を決めるキー自体。repo 側に置くと循環する |
| repo 可(host が上書き可) | `check_command`、`language`、`clean`、`pr.draft`(将来: `prompts` #149、`worktree_setup` #139) | プロジェクト内在。どのホストで回しても同じであるべき事実 |
| repo 可・要判断 | `schedules`(#146) | 反映には write 権限が要り境界内だが、常設の実行トリガを repo に置く段差があるため初期は host 側に据え置く(論点は issue #146 側) |

この原理により **workspace は host 一択で確定する**: workspace は定義上「他の repo 群を名指しする
グルーピング」なので、repo 内在の事実ではない。ADR 0009 の不変条件「スコープの拡大はホスト運用者のみ」が、
個別ルールから本原理の**帰結**に昇格する。

### セキュリティモデル(設計の芯)

1. **worktree のブランチからは絶対に読まない。default branch(trusted ref)から読む。**
   worktree から読むと、エージェントが自分のブランチで `check_command = "true"` に書き換えて
   完了契約(独立検証)を自己改竄できる。primary clone で trusted ref
   (`origin/<default_branch>`、無ければローカル `<default_branch>`)の
   `meguri.toml` blob を `git show` で解決する。verification は常にこの解決済み値を見るため、
   worktree 上の `meguri.toml` をいくら書き換えても検証には一切効かない。

2. **反映経路 = default branch へのマージ。** repo config の変更ゲートは、既存の人間マージゲート
   (draft PR 既定)/ branch protection がそのまま効く。「実行内容を決められる人 = write 権限者」という
   README のセキュリティモデルは不変。専用の承認機構は要らない。

3. **precedence は host が最後に勝つ**:
   `組み込み既定 < host グローバルセクション < repo meguri.toml < host [projects.*] override`。
   運用者はいつでもローカルで矯正できる。セクション wholesale 置換の既存流儀(`pr_for` / `clean_for`)を
   4 層に一般化する。

### 混入は静かに無視せず、エラーにする

host 専用キーを repo `meguri.toml` に書いても silent ignore しない。repo config のスキーマは
repo-eligible キーだけを持ち `deny_unknown_fields` 相当で拒否し、`meguri doctor` がエラーとして
報告する(routing と同じ「静かなフォールバックをしない」原則)。境界を曖昧に受け入れると、書いた人は
「効いている」と誤解する。

### 壊れた設定でプロセスを殺さない

parse / 検証に失敗した `meguri.toml` は、warn + イベント emit の上で**「無いもの扱い」**にフォールバック
し、host config のみで動作を継続する(`ConfigReloader` の「悪い設定でプロセスを殺さない」精神)。

## 帰結

- 同じ repo をどのホストで回しても、プロジェクト内在の設定は一致する。設定が repo と一緒に
  バージョン管理される。
- repo config は opt-in。`meguri.toml` を置かない既存プロジェクトの挙動は完全に不変。
- `[pr]` は「同一セクション内でキー単位に境界を持つ」最初の例になる(`draft` は repo 可、
  `auto_merge` は host 専用)。以降のセクションでもキー単位境界が必要なら本例に倣う。
- default branch の前進は既存の fetch サイクルで自然に追従するため、hot reload 通知機構は新設しない。

## 却下した代替案

- **worktree から読む(いかなる形でも)**: 完了契約の自己改竄を許すため却下。これがモデルの芯。
- **host 専用キーの repo 側 silent ignore**: 書いた人の誤解を生むため却下(doctor でエラー)。
- **repo config を host config より優先させる**: 運用者がローカルで矯正できなくなる。host が最後に勝つ。
- **`.meguri/meguri.toml` に置く**: `.meguri/` は git-exclude される runtime scratch と衝突する。repo ルート一択。
