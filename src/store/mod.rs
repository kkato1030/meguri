use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;

mod runs;
pub use runs::*;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    (
        "0002_agent_session",
        include_str!("migrations/0002_agent_session.sql"),
    ),
];

/// Thin handle over a single SQLite connection (WAL, busy-timeout).
///
/// meguri is a local single-writer tool; all DB calls are short. Callers on
/// the async runtime use `spawn_blocking` for anything hot, but plain calls
/// are acceptable for the tick-scale work we do.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open sqlite db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
               name TEXT PRIMARY KEY, applied_at TEXT NOT NULL
             );",
        )?;
        for (name, sql) in MIGRATIONS {
            let applied: bool = conn
                .prepare("SELECT 1 FROM schema_migrations WHERE name = ?1")?
                .exists([name])?;
            if applied {
                continue;
            }
            conn.execute_batch(sql)
                .with_context(|| format!("migration {name} failed"))?;
            conn.execute(
                "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
                rusqlite::params![name, now()],
            )?;
        }
        Ok(())
    }

    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }
}

/// RFC3339 UTC timestamp without external chrono dependency.
pub fn now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = d.as_secs();
    // Days-to-civil algorithm (Howard Hinnant), valid for the years we care about.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store.migrate().unwrap();
        store
            .with_conn(|c| {
                let n: i64 =
                    c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
                assert_eq!(n, MIGRATIONS.len() as i64);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn now_is_rfc3339_shaped() {
        let ts = now();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.starts_with("20"));
    }
}
