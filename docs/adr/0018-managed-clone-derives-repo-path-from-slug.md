# ADR 0018: repo_path は repo_slug からの導出値にする(managed bare clone)

- Status: proposed
- Date: 2026-07-14
- Issue: #195

## コンテキスト

これまで `[[projects]]` は `repo_path`(ユーザーが手で clone した working copy への
絶対パス)を必須にしてきた。meguri はそこへ `git fetch` / `git worktree add` を実行し、
ユーザーのリポジトリ状態を直接触る。そして `git clone` / `git init` / `remote add` は
ソース全体に一箇所も無い —「clone は既にある」という前提がハードコードされている。

ADR 0011 は host / repo の信頼境界を定め、`repo_path`・`repo_slug`・`id`・`default_branch` を
**host 専用の信頼宣言**(config への登録 = 信頼行為)として並べた。本 ADR はこの並びのうち
`repo_path` の扱いだけを更新する。信頼境界そのもの(どの repo を見るかは host が宣言し、repo に
語らせない)は動かさず、**その宣言を `repo_slug` に集約し、`repo_path` は宣言ではなく導出値に
する**。どこに clone があるかは、host が slug を宣言すれば meguri が一意に決められる事実であって、
人間が絶対パスで書き写す必要はない。

**この ADR が更新する既存判断**: ADR 0011 の host/repo 境界表で `repo_path` は「host 専用の
必須宣言」だった。本 ADR 以後、`repo_path` は host 専用のまま(信頼は動かない)だが **値は
optional・省略時は導出**になる。同表の `default_branch`(host 専用 bootstrap = 権威ブランチの
宣言)は **本 ADR では一切触らない** — その実測化は別の信頼境界変更なので、必要なら別 ADR で
ADR 0011 を明示的に supersede する(本 issue のスコープ外)。

## 決定

**github mode では `repo_path` を導出値にする。host が `repo_slug` を宣言すれば、
meguri が `~/.meguri/repos/<id>` に bare clone を実体化して所有する。**

- 信頼境界は変わらない。「どの repo を見るか」は host が宣言し、repo 自身には語らせない。
  宣言の綴りが絶対パスから slug になるだけである。
- `repo_path` の明示指定は従来どおり有効(既存ユーザーの移行パス)。指定があれば導出しない。
- **local mode は `repo_path` 手指定のまま。** clone 元(remote)が無いので導出できない
  (local mode に `repo_slug` が要らないのと同じ理由)。

### 置き場所と形式(なぜ bare か、なぜ worktree_root の外か)

- 管理 clone は `~/.meguri/repos/<id>` に **bare** で置く。
- **worktree_root(`~/.meguri/worktrees`)配下には置かない。** reaper が primary を
  刈らない唯一の防御は worktree_root プレフィックス比較である
  (`src/engine/reaper.rs` の `plan_with`)。配下に置くと誤マッチで刈られる。
- bare の利点: primary checkout が無い = ユーザーが触る余地も dirty になる余地もゼロ /
  checkout がブランチを保持しないので branch-held-by-checkout 系の競合が減る /
  working tree 分のディスクが要らない /「meguri 所有」がディレクトリ構造として自明。
- **`--mirror` は使わない。** mirror refspec(`+refs/*:refs/*`)だと `list_remote_branches`
  の `fetch --prune origin` が、`worktree add -b` で作った実行中の `meguri/*` ブランチを刈る。
- **fetch refspec を明示設定する。** `git clone --bare` は `remote.origin.fetch` を
  設定しないため `refs/remotes/origin/*` が更新されず、gitops の `origin/<default>` 参照が
  ローカルの古い ref に silent fallback する。clone 時に
  `remote.origin.fetch = +refs/heads/*:refs/remotes/origin/*` を設定し初回 fetch まで行う。

### clone は reconcile ステップ(ADR 0012)

「project が宣言されているのに clone が無い」は、ADR 0012 の level-triggered な乖離の一種で
あり、正常状態である。clone は worktree 作成と同族の reconcile ステップとして、tick ごとに
冪等に実体化する(既にあれば no-op)。clone 失敗(認証・ネットワーク・slug 誤記)は loud に
escalate し、silent skip しない。

### credential は gh に委譲

clone は `gh repo clone` 相当(gh の credential helper を継承)で行い、push の認証も従来どおり
gh に委譲する。forge が gh 完全依存である現状と一貫する。clone 所有が meguri に寄るぶん、
gh トークンの **write 権限**は doctor が事前に検査する(read-only トークンだと discovery は
通り push/PR で初めて落ちる、気づきにくい失敗を前倒しで検出する)。

## 帰結

- 最小構成が `id` + `repo_slug` の2行になる。手動 clone の手順が消える。
- git に触れるロジックは `src/gitops.rs` に集約する原則どおり、clone 関数を gitops に新設する。
  remote 名は既存 gitops 全関数のハードコードに合わせ必ず `origin`。
- `repo_path` が「設定値」から「導出値」に変わる。config の生の値は optional になり、
  実効パスは resolver 経由で解決する(`pr_for` / `deliver_for` と同じ `*_for` 慣習)。
- host の gh 認証情報でネットワーク越しに clone を作るという副作用が meguri に入る。
  それゆえ本 ADR に紐づく変更は「永続状態(ディスク上の clone)+ 公開契約(config schema)」に
  触れるものとして扱い、spec 側で migration / rollback を必須とする。
