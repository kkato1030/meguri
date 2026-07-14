use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;

mod panes;
mod reconcile;
mod runs;
mod schedules;
mod stats;
mod tasks;
pub use panes::*;
pub use runs::*;
pub use schedules::*;
pub use stats::*;
pub use tasks::*;
// `reconcile` only adds inherent `impl Store` methods (no exported types).

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    (
        "0002_heartbeats",
        include_str!("migrations/0002_heartbeats.sql"),
    ),
    (
        "0003_agent_session",
        include_str!("migrations/0003_agent_session.sql"),
    ),
    ("0004_panes", include_str!("migrations/0004_panes.sql")),
    (
        "0005_agent_profile",
        include_str!("migrations/0005_agent_profile.sql"),
    ),
    (
        "0006_pane_role",
        include_str!("migrations/0006_pane_role.sql"),
    ),
    // Renumbered from 0004 after the merge with main (which claimed 0004–0006):
    // this migration recreates the `runs` table, so it must run *after* every
    // other runs-touching migration (0005 adds `agent_profile`) to carry those
    // columns forward.
    ("0007_tasks", include_str!("migrations/0007_tasks.sql")),
    // routing 2/3 (#65): cli_versions + routing_drift. Independent new tables,
    // renumbered to 0008 after main claimed 0007; runs last so it sees the
    // recreated `runs` table from 0007_tasks.
    (
        "0008_routing_freshness",
        include_str!("migrations/0008_routing_freshness.sql"),
    ),
    // issue #142: reconcile — runs.body_digest + issue_reconcile. Renumbered to
    // 0009 after main claimed 0008 for routing freshness; independent tables.
    (
        "0009_reconcile",
        include_str!("migrations/0009_reconcile.sql"),
    ),
    // issue #146: schedule_state — cron schedule bookkeeping. Renumbered to
    // 0010 after main claimed 0008/0009; an independent new table.
    (
        "0010_schedules",
        include_str!("migrations/0010_schedules.sql"),
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

    /// Record that `name` (e.g. "watch") is alive right now.
    pub fn heartbeat(&self, name: &str) -> Result<()> {
        self.heartbeat_at(name, &now())
    }

    /// UPSERT a heartbeat with an explicit timestamp (tests fabricate stale ones).
    pub fn heartbeat_at(&self, name: &str, ts: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO heartbeats (name, ts) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET ts = excluded.ts",
                rusqlite::params![name, ts],
            )?;
            Ok(())
        })
    }

    pub fn latest_heartbeat(&self, name: &str) -> Result<Option<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT ts FROM heartbeats WHERE name = ?1")?;
            let mut rows = stmt.query([name])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
    }
}

/// Inverse of [`now`]: parse our RFC3339 UTC shape back to epoch seconds.
/// Returns None on anything that doesn't match `YYYY-MM-DDThh:mm:ssZ`.
pub fn parse_ts(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (year, month, day) = (num(&ts[0..4])?, num(&ts[5..7])?, num(&ts[8..10])?);
    let (h, m, s) = (num(&ts[11..13])?, num(&ts[14..16])?, num(&ts[17..19])?);
    // Days-from-civil (Howard Hinnant), the inverse of the algorithm in now().
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

/// RFC3339 UTC timestamp without external chrono dependency.
pub fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format_epoch(secs)
}

/// Format epoch seconds as our RFC3339 UTC shape (`YYYY-MM-DDThh:mm:ssZ`).
/// Split out from [`now`] so callers with an injected clock (e.g. the schedule
/// sweep, whose `now` is a test-supplied epoch) format the same way.
pub fn format_epoch(secs: u64) -> String {
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

    /// Apply the first `up_to` migrations onto a raw connection, in order.
    fn apply_migrations(conn: &Connection, up_to: usize) {
        for (_, sql) in &MIGRATIONS[..up_to] {
            conn.execute_batch(sql).unwrap();
        }
    }

    #[test]
    fn migration_tasks_preserves_runs_and_splits_the_active_index() {
        // Acceptance criterion 7: a DB already at the prior migration with runs
        // data survives the tasks migration with its data and active-run
        // exclusion intact. (0007 after the merge with main, which took 0004–0006.)
        let conn = Connection::open_in_memory().unwrap();
        let idx_tasks = MIGRATIONS
            .iter()
            .position(|(n, _)| *n == "0007_tasks")
            .unwrap();
        apply_migrations(&conn, idx_tasks); // everything before the tasks migration

        // A pre-existing github run.
        conn.execute(
            "INSERT INTO runs (id, project_id, loop_kind, issue_number, issue_title,
                               status, created_at)
             VALUES ('run-old', 'proj', 'worker', 7, 'old', 'running', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        // Apply the tasks migration (recreates runs, adds tasks + partial indexes).
        conn.execute_batch(MIGRATIONS[idx_tasks].1).unwrap();

        // Data survived, and issue_number maps through with task_id NULL.
        let (issue, task): (i64, Option<i64>) = conn
            .query_row(
                "SELECT issue_number, task_id FROM runs WHERE id = 'run-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(issue, 7);
        assert_eq!(task, None);

        // runs_active_issue: a second active run for the same (project, loop,
        // issue) is rejected.
        let dup = conn.execute(
            "INSERT INTO runs (id, project_id, loop_kind, issue_number, status, created_at)
             VALUES ('run-dup', 'proj', 'worker', 7, 'queued', '2026-01-01T00:00:00Z')",
            [],
        );
        assert!(dup.is_err(), "duplicate active issue run must be rejected");

        // A local run (task_id set, issue_number NULL) lives under the other
        // partial index and does not collide with issue runs.
        conn.execute(
            "INSERT INTO runs (id, project_id, loop_kind, task_id, status, created_at)
             VALUES ('run-loc', 'proj', 'worker', 3, 'queued', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let dup_task = conn.execute(
            "INSERT INTO runs (id, project_id, loop_kind, task_id, status, created_at)
             VALUES ('run-loc2', 'proj', 'worker', 3, 'queued', '2026-01-01T00:00:00Z')",
            [],
        );
        assert!(
            dup_task.is_err(),
            "duplicate active task run must be rejected"
        );

        // Two active issue-NULL runs for different tasks coexist (the partial
        // index keys on task_id, not the NULL issue_number).
        conn.execute(
            "INSERT INTO runs (id, project_id, loop_kind, task_id, status, created_at)
             VALUES ('run-loc3', 'proj', 'worker', 4, 'queued', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn now_is_rfc3339_shaped() {
        let ts = now();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.starts_with("20"));
    }

    #[test]
    fn parse_ts_inverts_now() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let parsed = parse_ts(&now()).unwrap();
        assert!(parsed.abs_diff(before) <= 1);
        assert_eq!(parse_ts("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_ts("not a timestamp"), None);
        assert_eq!(parse_ts("2000-03-01 00:00:00Z"), None);
    }

    #[test]
    fn heartbeat_upsert_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.latest_heartbeat("watch").unwrap(), None);

        store.heartbeat_at("watch", "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            store.latest_heartbeat("watch").unwrap().as_deref(),
            Some("2026-01-01T00:00:00Z")
        );

        // UPSERT overwrites the single row instead of accumulating.
        store.heartbeat("watch").unwrap();
        let ts = store.latest_heartbeat("watch").unwrap().unwrap();
        assert_ne!(ts, "2026-01-01T00:00:00Z");
        store
            .with_conn(|c| {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))?;
                assert_eq!(n, 1);
                Ok(())
            })
            .unwrap();
    }
}
