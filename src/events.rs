use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use crate::store::{Store, now};

#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    pub id: i64,
    pub ts: String,
    pub run_id: Option<String>,
    pub kind: String,
    pub data: Value,
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let data: String = row.get(4)?;
    Ok(EventRecord {
        id: row.get(0)?,
        ts: row.get(1)?,
        run_id: row.get(2)?,
        kind: row.get(3)?,
        data: serde_json::from_str(&data).unwrap_or(Value::Null),
    })
}

impl Store {
    pub fn emit(&self, run_id: Option<&str>, kind: &str, data: Value) -> Result<()> {
        self.emit_at(run_id, kind, data, &now())
    }

    /// Like [`Store::emit`] but with an explicit timestamp — tests fabricate
    /// events outside the `events_since` window (the same pattern
    /// [`Store::heartbeat_at`] uses for a stale heartbeat).
    pub fn emit_at(&self, run_id: Option<&str>, kind: &str, data: Value, ts: &str) -> Result<()> {
        tracing::info!(run_id, kind, %data, "event");
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO events (ts, run_id, kind, data_json) VALUES (?1, ?2, ?3, ?4)",
                params![ts, run_id, kind, data.to_string()],
            )?;
            Ok(())
        })
    }

    pub fn events_for_run(&self, run_id: &str, limit: usize) -> Result<Vec<EventRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ts, run_id, kind, data_json FROM events
                 WHERE run_id = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let mut events: Vec<EventRecord> = stmt
                .query_map(params![run_id, limit as i64], event_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            events.reverse();
            Ok(events)
        })
    }

    /// Count events of a given kind (run-scoped or not). Used by drift
    /// journalling tests and health summaries — drift events carry no run_id,
    /// so `events_for_run` can't reach them.
    pub fn count_events(&self, kind: &str) -> Result<usize> {
        self.with_conn(|c| {
            let n: i64 =
                c.query_row("SELECT COUNT(*) FROM events WHERE kind = ?1", [kind], |r| {
                    r.get(0)
                })?;
            Ok(n as usize)
        })
    }

    /// Events of `kind` at or after `since_ts` (RFC3339, string-sortable like
    /// every other `ts` column) — the read side of `meguri doctor`'s sweep
    /// failure-rate display (issue #251, design doc P6.5 item 3). Global, not
    /// run-scoped: sweep events carry no `run_id`.
    pub fn events_since(&self, kind: &str, since_ts: &str) -> Result<Vec<EventRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ts, run_id, kind, data_json FROM events
                 WHERE kind = ?1 AND ts >= ?2 ORDER BY id ASC",
            )?;
            let events = stmt
                .query_map(params![kind, since_ts], event_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(events)
        })
    }

    /// Events with id > `after_id`, id-ascending — the polling cursor for the
    /// web UI (`after=0` returns from the beginning).
    pub fn events_for_run_after(
        &self,
        run_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ts, run_id, kind, data_json FROM events
                 WHERE run_id = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3",
            )?;
            let events = stmt
                .query_map(params![run_id, after_id, limit as i64], event_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(events)
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn events_for_run_after_pages_by_id_cursor() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        for i in 0..5 {
            store
                .emit(Some(&run.id), "test.tick", json!({ "i": i }))
                .unwrap();
        }
        store.emit(None, "global.noise", json!({})).unwrap();

        let all = store.events_for_run_after(&run.id, 0, 100).unwrap();
        assert_eq!(all.len(), 5, "run-scoped only");
        assert!(all.windows(2).all(|w| w[0].id < w[1].id), "id ascending");

        let cursor = all[2].id;
        let rest = store.events_for_run_after(&run.id, cursor, 100).unwrap();
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].id, all[3].id);

        let limited = store.events_for_run_after(&run.id, 0, 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].id, all[0].id);
    }

    #[test]
    fn events_since_filters_by_ts_and_kind() {
        let store = Store::open_in_memory().unwrap();
        store
            .emit_at(
                None,
                "sweep.failed",
                json!({ "sweep": "merge-tail" }),
                "2026-07-21T00:00:00Z",
            )
            .unwrap();
        store
            .emit_at(
                None,
                "sweep.failed",
                json!({ "sweep": "merge-tail" }),
                "2026-07-21T02:00:00Z",
            )
            .unwrap();
        // A different kind at the same ts must not be counted.
        store
            .emit_at(None, "sweep.degraded", json!({}), "2026-07-21T02:00:00Z")
            .unwrap();

        let recent = store
            .events_since("sweep.failed", "2026-07-21T01:00:00Z")
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].data["sweep"], "merge-tail");
    }
}
