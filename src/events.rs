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
}
