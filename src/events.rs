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
        tracing::info!(run_id, kind, %data, "event");
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO events (ts, run_id, kind, data_json) VALUES (?1, ?2, ?3, ?4)",
                params![now(), run_id, kind, data.to_string()],
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

    /// How many `infra.raised` events name this target since `since_ts` — the
    /// fault window backing the infra retry cap (issue #250): a permanently
    /// broken mux/gh must eventually reach a human instead of retrying (and
    /// growing runs, worktrees and API spend) forever.
    pub fn infra_raised_since(&self, target: &str, id: i64, since_ts: &str) -> Result<usize> {
        self.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM events
                 WHERE kind = 'infra.raised' AND ts >= ?1
                   AND json_extract(data_json, '$.target') = ?2
                   AND json_extract(data_json, '$.id') = ?3",
                params![since_ts, target, id],
                |r| r.get(0),
            )?;
            Ok(n as usize)
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
}
