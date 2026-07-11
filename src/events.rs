use anyhow::Result;
use rusqlite::params;
use serde_json::Value;

use crate::store::{Store, now};

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub ts: String,
    pub run_id: Option<String>,
    pub kind: String,
    pub data: Value,
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
                "SELECT ts, run_id, kind, data_json FROM events
                 WHERE run_id = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let mut events: Vec<EventRecord> = stmt
                .query_map(params![run_id, limit as i64], |row| {
                    let data: String = row.get(3)?;
                    Ok(EventRecord {
                        ts: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        data: serde_json::from_str(&data).unwrap_or(Value::Null),
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            events.reverse();
            Ok(events)
        })
    }
}
