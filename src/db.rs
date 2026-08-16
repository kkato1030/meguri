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
            let h = std::env::var_os("HOME").context("HOME が未設定")?;
            PathBuf::from(h).join(".meguri")
        }
    };
    std::fs::create_dir_all(&home)
        .with_context(|| format!("meguri home を作れない: {}", home.display()))?;
    Ok(home)
}

/// DB ファイルの場所。ホーム配下の `meguri.db`。
fn db_path() -> Result<PathBuf> {
    Ok(meguri_home()?.join("meguri.db"))
}

/// DB を開き、スキーマを用意して返す。
pub fn open() -> Result<Connection> {
    let path = db_path()?;
    let conn = Connection::open(&path)
        .with_context(|| format!("DB を開けない: {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate(&conn)?;
    Ok(conn)
}

/// スキーマ(冪等)。増分で列を足すときはここに追記していく。
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// スキーマ SQL。テストの in-memory DB でも使う。
pub(crate) const SCHEMA: &str = r#"
        CREATE TABLE IF NOT EXISTS intents (
            id          INTEGER PRIMARY KEY,
            title       TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS outcomes (
            id              INTEGER PRIMARY KEY,
            intent_id       INTEGER NOT NULL REFERENCES intents(id),
            statement       TEXT NOT NULL,
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
            id        INTEGER PRIMARY KEY,
            serves_id INTEGER NOT NULL REFERENCES outcomes(id),
            objective TEXT NOT NULL,
            executor  TEXT NOT NULL DEFAULT 'ai',       -- 'ai' | 'human'
            state     TEXT NOT NULL DEFAULT 'planned'
        );
        "#;
