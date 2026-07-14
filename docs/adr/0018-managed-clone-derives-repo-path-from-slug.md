# ADR 0018: repo_path は repo_slug からの導出値にする(managed bare clone)

- Status: proposed
- Date: 2026-07-14
- Issue: #195

## コンテキスト

これまで `[[projects]]` は `repo_path`(ユーザーが手で clone した working copy への
絶対パス)を必須にしてきた。meguri はそこへ `git fetch` / `git worktree add` を実行し、
ユーザーのリポジトリ状態を直接触る。そして `git clone` / `git init` / `remote add` は
ソース全体に一箇所も無い —「clone は既にある」という前提がハードコードされている。

ADR 0013 は「config が観測可能な現実を手書きで重複するのはドリフト源」として、
`repo_slug` と `default_branch` を導出側に倒した。同じ理屈が `repo_path` にも効く。
どこに clone があるかは、host が slug を宣言すれば meguri が一意に決められる事実であって、
人間が絶対パスで書き写す必要はない。

## 決定

**github mode では `repo_path` を導出値にする。host が `repo_slug` を宣言すれば、
meguri が `~/.meguri/repos/<id>` に bare clone を実体化して所有する。**

- 信頼境界は変わらない。「どの repo を見るか」は host が宣言し、repo 自身には語らせない。
  宣言の綴りが絶対パスから slug になるだけである。
- `repo_path` の明示指定は従来どおり有効(既存ユーザーの移行パス)。指定があれば導出しない。
- **local mode は `repo_path` 手指定のまま。** clone 元(remote)が無いので導出できない。
  ADR 0013 が local mode に `default_branch` / `repo_slug` を残置したのと同型の分岐。

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
  実効パスは resolver 経由で解決する(ADR 0013 の `repo_slug` 導出と同じ設計)。
- host の gh 認証情報でネットワーク越しに clone を作るという副作用が meguri に入る。
  それゆえ本 ADR に紐づく変更は「永続状態(ディスク上の clone)+ 公開契約(config schema)」に
  触れるものとして扱い、spec 側で migration / rollback を必須とする。
