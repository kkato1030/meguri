# ADR 0011: 二層 config — プロジェクト内在の設定は repo ルート `meguri.toml` に宣言し、run 開始時に worktree から読んで pin する

- Status: proposed
- Date: 2026-07-14
- Issue: #165

## コンテキスト

per-project の設定が host 側 `~/.meguri/config.toml` の `[[projects]]` に増殖している
(`check_command` / `language` / `clean.ignore` / `pr` …)。計画中の #149(prompts)・
#146(schedules)・#139(worktree_setup)もいずれも「repo 相対パスや repo 内の運用規律を
host config から指す」形で、**プロジェクト内在の設定が repo の外に住んでいる**歪みが顕在化した。
同じ repo を別ホストで回すと設定を手で複製することになり、repo と一緒にバージョン管理もされない。

一方で、「設定を repo に置く」ことには固有の懸念がある。meguri の完了契約は独立検証
(`check_command` を meguri 自身が回す)に立脚しており(README の Security 参照)、その検証内容を
run 中のエージェントが**自分の作業ツリーで書き換えられる**と、契約を自己改竄されうる。

同時に #154(workspace)の「置き場所は repo 側か host 側か」という論点も未決だった。

### 一度は検討して却下した「trusted ref から読む」案

初版の本 ADR は「worktree ではなく default branch(trusted ref)の `meguri.toml` を
`git show origin/<default_branch>:meguri.toml` で読めば、worktree 改竄は検証に効かない」という
セキュリティモデルを芯に据えていた。しかしこれは**現行アーキテクチャでは保証にならない**:

- meguri の worktree は linked worktree(`git worktree add`)で作られ、primary clone と
  **git dir(refs)を共有する**。
- したがって agent は worktree 内から
  `git update-ref refs/remotes/origin/<default_branch> <悪意 commit>` で trusted ref 自体を
  書き換えられる(meguri 自身 `conflict_resolver` で `update-ref` を使う = agent も当然使える)。
- spec は追加 fetch しない方針だったため、この local ref 改竄を矯正できず、primary clone の
  `git show origin/<default_branch>:meguri.toml` は未マージの内容を返す。

つまり trusted ref 案は、改竄経路を「worktree のファイル編集」から「plumbing 一発」に変えるだけで、
**保証を与えないどころか偽の安心を与える**。この教訓が本 ADR の設計を決めた。

## 決定

repo ルートに `meguri.toml` を置けるようにし、**プロジェクト内在の設定を repo 側で宣言できる**
ようにする。読み方は次のとおり。

> **repo config は run の worktree から読む。ただし run claim 時(初回の worktree 準備時)に一度だけ
> 読んで run に pin し、以後その run はファイル編集にも ref 改竄にも crash→resume にも動じず pin 値を使う。**

どのキーを repo に置けるかは、次の**境界原理**で二分する。

> **repo config に書けるのは、その repo 自身の run にしか影響しない「プロジェクト内在の事実」だけ。
> 他の repo を名指しする設定・ホストマシンや token に束縛される設定・信頼の宣言は host config のみ。**

この一つの原理が、個別に決めていた分類を演繹に変える。

### キーの分類

| 層 | キー | 理由 |
|---|---|---|
| host 専用 | `[[workspaces]]`、`id`、`repo_path`、`repo_slug`、`mode`、`deliver`、`worktree_root` | 他 repo の名指し / マシン・token 束縛 / 信頼の宣言(config への登録 = 信頼行為) |
| host 専用 | `mux`、`agent(s)`、`routing`、`drift`、`limits`、`scheduler`、`daemon`、`notifications`、`pr.auto_merge` | ホスト運用者のリソース・権限。auto_merge は host の `gh` token でマージを arm するため保守的に host 側 |
| host 専用(bootstrap) | `default_branch` | 新規 worktree の base / PR の base / 「どのブランチが権威か」の宣言。repo 側に置くと信頼の自己宣言になる |
| repo 可(host が上書き可) | `check_command`、`language`、`clean`、`pr.draft`(将来: `prompts` #149、`worktree_setup` #139) | プロジェクト内在。どのホストで回しても同じであるべき事実 |
| repo 可・要判断 | `schedules`(#146) | 反映には write 権限が要り境界内だが、常設の実行トリガを repo に置く段差があるため初期は host 側に据え置く(論点は issue #146 側) |

この原理により **workspace は host 一択で確定する**: workspace は定義上「他の repo 群を名指しする
グルーピング」なので、repo 内在の事実ではない。ADR 0009 の不変条件「スコープの拡大はホスト運用者のみ」が、
個別ルールから本原理の**帰結**に昇格する。

### セキュリティモデル(設計の芯)

**前提となる脅威モデル**: meguri が守るのは「敵対的 agent のサンドボックス隔離」ではなく、
**「fallible(非敵対的)な agent に対する guardrail」**である。agent は worktree 内で host の
credential 付きで任意コマンドを実行でき、真に敵対的なら token 窃取・任意 push も原理的に可能で、
config 整合性だけを守っても最後の砦にならない(そもそも同一 host・共有 git dir 上で任意コマンドを
実行できる相手に対する config 整合性の完全保証は不能 — 上の「却下した trusted ref 案」参照)。
守るべきは現実的な失敗モード、すなわち **「LLM が行き詰まった時に、自分の完了契約を *自然な
in-band 編集* で緩める」**(例: `check_command = "true"` に書き換える)ことである。

1. **claim 時 pin。** 完了契約を含む repo-eligible 値は、run claim 時(初回 worktree 準備時)に
   worktree の `meguri.toml` から一度だけ解決し、run の checkpoint に pin する。以後その run の
   検証・PR 作成・prompt はこの pin 値だけを読む。
   - agent が run 中に worktree の `meguri.toml` を書き換えても、その run の検証には効かない。
   - `update-ref` 等で ref を弄っても、そもそも ref を読まないので効かない。
   - crash → resume でも **pin 値を再利用し worktree を再読しない**(resume 時の再読は改竄経路に
     なるため、これは意図的)。
   - 既存の `base_sha` / `agent_profile` / `body_digest` の「claim 時に settle し resume 間で
     不変」idiom(`Checkpoint`)にそのまま乗せる。新規機構ではない。

2. **保証範囲(正直な線引き)。** 保証するのは *「開始済み run の完了契約は claim 後に不変」* まで。
   pin される値の出所は claim 時点で worktree に checkout されているブランチの内容である:
   - **新規タスク run** は worktree を `origin/<default_branch>`(無ければローカル
     `<default_branch>`)から分岐して作るので、claim 時点の `meguri.toml` ≒ default branch の内容。
     実質「default branch にマージされた設定でその run が回る」。
   - **PR ブランチに attach する run / 既存 worktree を resume する run** は、その PR ブランチ自身の
     `meguri.toml` を pin する。これは config-with-code として意図どおり(その PR が提案する設定で
     その PR を検証する)。前段の run が commit した弱い `check_command` を honor しうるが、それは
     **PR diff に現れ、人間マージゲート(draft PR 既定)/ branch protection のレビュー対象になる**。
     trusted ref に対する `update-ref` 改竄が diff に出ず不可視だったのと対照的に、**worktree 方式は
     改竄が監査可能**である。
   - 同一 host・共有 git dir で任意コマンドを実行できる敵対的 agent に対する config 整合性の完全保証は
     本 ADR の対象外。将来サンドボックス(config 読み取り専用の別 git dir + remote 再 fetch)を
     導入する場合に別途扱う(現行アーキはそこに立っていない — YAGNI)。

3. **反映経路 = そのブランチへの commit。** default branch にマージされた `meguri.toml` は以後の
   新規 run に効き、PR ブランチの `meguri.toml` はその PR の run に効く。いずれも commit には write
   権限が要るので、「実行内容を決められる人 = write 権限者」という README のモデルの内側。専用の
   承認機構は要らない。

4. **precedence は host が最後に勝つ**:
   `組み込み既定 < host グローバルセクション < repo meguri.toml(claim 時 pin)< host [projects.*] override`。
   運用者はいつでもローカルで矯正できる。セクション wholesale 置換の既存流儀(`pr_for` / `clean_for`)を
   4 層に一般化する。

### 混入は静かに無視せず、エラーにする

host 専用キーを repo `meguri.toml` に書いても silent ignore しない。repo config のスキーマは
repo-eligible キーだけを持ち `deny_unknown_fields` 相当で拒否し、`meguri doctor` がエラーとして
報告する(routing と同じ「静かなフォールバックをしない」原則)。境界を曖昧に受け入れると、書いた人は
「効いている」と誤解する。

### 壊れた設定でプロセスを殺さない

parse / 検証に失敗した `meguri.toml` は、warn + イベント emit の上で**「無いもの扱い」**にフォールバック
し、host config のみでその run を継続する(`ConfigReloader` の「悪い設定でプロセスを殺さない」精神)。

## 帰結

- 同じ repo をどのホストで回しても、プロジェクト内在の設定は一致する。設定が repo と一緒に
  バージョン管理される。
- repo config は opt-in。`meguri.toml` を置かない既存プロジェクトの挙動は完全に不変。
- `[pr]` は「同一セクション内でキー単位に境界を持つ」最初の例になる(`draft` は repo 可、
  `auto_merge` は host 専用)。以降のセクションでもキー単位境界が必要なら本例に倣う。
- 反映は「そのブランチへの commit」で完結し、trusted ref 追従のための hot reload / fetch cadence 機構は
  不要(claim 時に worktree を一度読むだけ)。
- 設定の効き方が直感的になる: default branch にマージすれば以後の run に、PR ブランチに commit すれば
  その PR の run に効く(config-with-code)。

## 却下した代替案

- **trusted ref(default branch)から読む**: linked worktree が git dir を共有するため
  `git update-ref` で迂回でき、保証にならないどころか偽の安心を与える(本 ADR「コンテキスト」参照)。
  重い ref 分離(別 git dir + remote 再 fetch)を積めば塞げるが、現行アーキに乗らず YAGNI。
- **worktree を run 中ずっと live に読む(pin しない)**: agent が run 中に `check_command` を緩めて
  自己改竄できるため却下。pin が要る。
- **resume 時に worktree を再読する**: 改竄経路になるため却下。resume は checkpoint の pin を使う。
- **host 専用キーの repo 側 silent ignore**: 書いた人の誤解を生むため却下(doctor でエラー)。
- **repo config を host config より優先させる**: 運用者がローカルで矯正できなくなる。host が最後に勝つ。
- **`.meguri/meguri.toml` に置く**: `.meguri/` は git-exclude される runtime scratch と衝突する。repo ルート一択。
