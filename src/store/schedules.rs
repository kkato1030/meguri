//! The `schedule_state` table: per-schedule cron bookkeeping (issue #146). The
//! firing logic lives out-of-band in [`engine::schedule`](crate::engine::schedule);
//! this module only persists the "last consumed window" and the last-created
//! issue/task key, so a restart folds catch-up to a single fire and the
//! overlap guard has a cheap openness check to make.

use anyhow::Result;
use rusqlite::{Row, params};

use super::Store;

/// One `schedule_state` row. Mirrors the columns in `0008_schedules.sql`.
#[derive(Debug, Clone)]
pub struct ScheduleStateRow {
    pub project_id: String,
    pub name: String,
    pub first_seen_at: String,
    pub last_fired_at: Option<String>,
    pub last_key: Option<i64>,
}

fn schedule_from_row(r: &Row<'_>) -> rusqlite::Result<ScheduleStateRow> {
    Ok(ScheduleStateRow {
        project_id: r.get("project_id")?,
        name: r.get("name")?,
        first_seen_at: r.get("first_seen_at")?,
        last_fired_at: r.get("last_fired_at")?,
        last_key: r.get("last_key")?,
    })
}

impl Store {
    /// The persisted state for one schedule, or `None` before its first
    /// observation.
    pub fn get_schedule_state(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<Option<ScheduleStateRow>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM schedule_state WHERE project_id = ?1 AND name = ?2")?;
            let mut rows = stmt.query(params![project_id, name])?;
            match rows.next()? {
                Some(r) => Ok(Some(schedule_from_row(r)?)),
                None => Ok(None),
            }
        })
    }

    /// Record the first observation of a schedule without firing (the backfill
    /// guard). A no-op if a row already exists.
    pub fn seed_schedule(&self, project_id: &str, name: &str, seen_at: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO schedule_state (project_id, name, first_seen_at)
                 VALUES (?1, ?2, ?3)",
                params![project_id, name, seen_at],
            )?;
            Ok(())
        })
    }

    /// Advance the consumed window to `fired_at`. `last_key = Some` on a real
    /// enqueue (records the created issue/task); `None` on an overlap skip
    /// (the window is still consumed, but the previous key is kept).
    pub fn record_schedule_fire(
        &self,
        project_id: &str,
        name: &str,
        fired_at: &str,
        last_key: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE schedule_state SET last_fired_at = ?3,
                   last_key = CASE WHEN ?4 IS NOT NULL THEN ?4 ELSE last_key END
                 WHERE project_id = ?1 AND name = ?2",
                params![project_id, name, fired_at, last_key],
            )?;
            Ok(())
        })
    }

    /// Every schedule-state row for a project (for `meguri schedules`).
    pub fn list_schedule_state(&self, project_id: &str) -> Result<Vec<ScheduleStateRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM schedule_state WHERE project_id = ?1")?;
            let rows = stmt
                .query_map(params![project_id], schedule_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_is_idempotent_and_does_not_clobber() {
        let store = Store::open_in_memory().unwrap();
        store
            .seed_schedule("p", "daily", "2026-07-13T00:00:00Z")
            .unwrap();
        // A second seed must not reset first_seen_at.
        store
            .seed_schedule("p", "daily", "2026-07-14T00:00:00Z")
            .unwrap();
        let row = store.get_schedule_state("p", "daily").unwrap().unwrap();
        assert_eq!(row.first_seen_at, "2026-07-13T00:00:00Z");
        assert_eq!(row.last_fired_at, None);
        assert_eq!(row.last_key, None);
    }

    #[test]
    fn record_fire_sets_key_and_skip_keeps_it() {
        let store = Store::open_in_memory().unwrap();
        store
            .seed_schedule("p", "daily", "2026-07-13T00:00:00Z")
            .unwrap();

        // A real enqueue records the key and advances the window.
        store
            .record_schedule_fire("p", "daily", "2026-07-13T09:00:00Z", Some(42))
            .unwrap();
        let row = store.get_schedule_state("p", "daily").unwrap().unwrap();
        assert_eq!(row.last_fired_at.as_deref(), Some("2026-07-13T09:00:00Z"));
        assert_eq!(row.last_key, Some(42));

        // A skip advances the window but keeps the previous key.
        store
            .record_schedule_fire("p", "daily", "2026-07-14T09:00:00Z", None)
            .unwrap();
        let row = store.get_schedule_state("p", "daily").unwrap().unwrap();
        assert_eq!(row.last_fired_at.as_deref(), Some("2026-07-14T09:00:00Z"));
        assert_eq!(row.last_key, Some(42));
    }

    #[test]
    fn list_is_scoped_by_project() {
        let store = Store::open_in_memory().unwrap();
        store
            .seed_schedule("p", "a", "2026-07-13T00:00:00Z")
            .unwrap();
        store
            .seed_schedule("p", "b", "2026-07-13T00:00:00Z")
            .unwrap();
        store
            .seed_schedule("q", "a", "2026-07-13T00:00:00Z")
            .unwrap();
        assert_eq!(store.list_schedule_state("p").unwrap().len(), 2);
        assert_eq!(store.list_schedule_state("q").unwrap().len(), 1);
    }
}
