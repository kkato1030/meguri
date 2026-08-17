//! sqlite 接続とスキーマ。**保存するのは事実のみ**(§15):
//! Intent / Outcome / requires 辺 / Work / human 充足表明。
//! satisfied・ready・blocked といった導出値は保存しない(§5、derive.rs で毎回導出)。

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// meguri のホーム(`MEGURI_HOME`、既定 `~/.meguri`)。無ければ作る。
pub fn meguri_home() -> Result<PathBuf> {
    let home = match std::env::var_os("MEGURI_HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            let h = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(h).join(".meguri")
        }
    };
    std::fs::create_dir_all(&home)
        .with_context(|| format!("cannot create meguri home: {}", home.display()))?;
    Ok(home)
}

/// DB ファイルの場所。ホーム配下の `meguri.db`。
fn db_path() -> Result<PathBuf> {
    Ok(meguri_home()?.join("meguri.db"))
}

/// 管理 repo の bare clone を置くディレクトリ(`MEGURI_HOME/repos`)。
pub fn repos_dir() -> Result<PathBuf> {
    Ok(meguri_home()?.join("repos"))
}

/// worktree を置くディレクトリ(`MEGURI_HOME/worktrees`)。o14(spawn)で使う。
#[allow(dead_code)]
pub fn worktrees_dir() -> Result<PathBuf> {
    Ok(meguri_home()?.join("worktrees"))
}

/// DB を開き、スキーマを用意して返す。
pub fn open() -> Result<Connection> {
    let path = db_path()?;
    let conn = Connection::open(&path)
        .with_context(|| format!("cannot open DB: {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate(&conn)?;
    Ok(conn)
}

/// スキーマ(冪等)。新規 DB は SCHEMA で作り、既存 DB は列追加で追随する。
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    // 既存 DB 向けの列追加(新規 DB は SCHEMA に含むので二重にならないよう存在確認)。
    add_column_if_missing(conn, "outcomes", "description", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "intents", "repo_id", "INTEGER")?;
    add_column_if_missing(conn, "works", "worktree_path", "TEXT")?;
    add_column_if_missing(conn, "works", "branch", "TEXT")?;
    add_column_if_missing(conn, "works", "base_sha", "TEXT")?;
    add_column_if_missing(conn, "works", "artifact_sha", "TEXT")?; // o21: verified な commit
    add_column_if_missing(conn, "works", "pane_id", "TEXT")?; // watch が nudge するための pane ハンドル
    Ok(())
}

/// テーブルに列が無ければ足す(冪等な最小マイグレーション)。
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|c| c.ok())
        .any(|c| c == column);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}

/// スキーマ SQL。テストの in-memory DB でも使う。
pub(crate) const SCHEMA: &str = r#"
        -- meguri が管理する repo(bare clone は MEGURI_HOME/repos/<name>.git)。
        CREATE TABLE IF NOT EXISTS repos (
            id             INTEGER PRIMARY KEY,
            name           TEXT NOT NULL UNIQUE,
            origin         TEXT NOT NULL,
            default_branch TEXT NOT NULL DEFAULT 'main'
        );

        CREATE TABLE IF NOT EXISTS intents (
            id          INTEGER PRIMARY KEY,
            title       TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            repo_id     INTEGER REFERENCES repos(id)   -- どの repo で実行するか(任意)
        );

        CREATE TABLE IF NOT EXISTS outcomes (
            id              INTEGER PRIMARY KEY,
            intent_id       INTEGER NOT NULL REFERENCES intents(id),
            statement       TEXT NOT NULL,
            description     TEXT NOT NULL DEFAULT '',
            -- verify.kind: 'command'(コマンド exit 0)| 'human'(人が表明)| 'rollup'(まとめ節点)
            verify_kind     TEXT NOT NULL DEFAULT 'human',
            verify_command  TEXT,          -- kind=command のときのみ
            -- human 充足表明(sticky)。kind=human の satisfied 判定に使う事実。
            human_satisfied INTEGER NOT NULL DEFAULT 0
        );

        -- requires 辺: outcome_id は requires_id を前提とする(requires_id が先)。
        CREATE TABLE IF NOT EXISTS outcome_requires (
            outcome_id  INTEGER NOT NULL REFERENCES outcomes(id),
            requires_id INTEGER NOT NULL REFERENCES outcomes(id),
            PRIMARY KEY (outcome_id, requires_id)
        );

        CREATE TABLE IF NOT EXISTS works (
            id            INTEGER PRIMARY KEY,
            serves_id     INTEGER NOT NULL REFERENCES outcomes(id),
            objective     TEXT NOT NULL,
            executor      TEXT NOT NULL DEFAULT 'ai',   -- 'ai' | 'human'
            state         TEXT NOT NULL DEFAULT 'planned',
            worktree_path TEXT,                          -- spawn 時に埋まる
            branch        TEXT,
            base_sha      TEXT,
            artifact_sha  TEXT,                           -- verified な commit(o21)
            pane_id       TEXT                            -- watch が nudge するための pane ハンドル
        );
        "#;
