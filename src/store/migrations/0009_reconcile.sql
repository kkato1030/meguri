-- reconcile loop が issue 本文の編集を検知する (issue #142)。
--
-- runs に body_digest を足す: その run が処理した正規化本文の SHA-256 で、
-- checkpoint.issue_body が確定した直後に記録する。#142 以前の run は NULL の
-- ままで、「どの本文にも一致」= 従来どおりの恒久サプレッションとして扱う
-- (アップグレード時に既処理 issue が一斉に再ディスカバリされる暴発を防ぐ)。
-- nullable な列の追加なので ALTER ADD COLUMN で足りる(既存行のバックフィル不要)。
ALTER TABLE runs ADD COLUMN body_digest TEXT;

-- reconcile loop のシグナル dedup: (project, issue, 新本文ダイジェスト) ごとに
-- issue.body_changed イベント + シグナルコメントを高々一度だけにする。half A
-- (discover guard)と half B(poll sweep)がイベント発火前にこの行で gate する
-- ので、本文が編集されてからまだ再処理されていない間もイベントが毎 tick 積み増し
-- されない(受け入れ基準 2 の振動防止)。
CREATE TABLE issue_reconcile (
  project_id TEXT NOT NULL,
  issue_number INTEGER NOT NULL,
  signaled_digest TEXT NOT NULL,
  signaled_at TEXT NOT NULL,
  PRIMARY KEY (project_id, issue_number)
);
