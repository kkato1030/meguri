//! Direct ndjson client for herdr's local socket API.
//!
//! The herdr CLI is a thin wrapper over this socket; talking to it directly
//! avoids a fork/exec per request and unlocks `events.subscribe`, which the
//! CLI does not expose. Error mapping doubles as fallback routing for
//! callers: `MuxError::Io` means the socket transport is unusable (try the
//! CLI instead), `MuxError::CommandFailed` means herdr answered with an
//! error (authoritative — a CLI retry would say the same).

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;

use super::{MuxError, MuxResult};

/// Bound on connect + one request/response round trip, so a wedged herdr
/// server cannot stall the turn engine.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// One-shot request; returns the response's `result` value.
pub(crate) async fn request(socket: &Path, method: &str, params: Value) -> MuxResult<Value> {
    let fut = async {
        let mut stream = UnixStream::connect(socket).await?;
        let line = serde_json::to_string(&json!({
            "id": format!("meguri:{method}"),
            "method": method,
            "params": params,
        }))
        .expect("request serializes");
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        if reader.read_line(&mut resp).await? == 0 {
            return Err(eof_error(method));
        }
        parse_response(resp.trim(), method)
    };
    tokio::time::timeout(REQUEST_TIMEOUT, fut)
        .await
        .map_err(|_| timeout_error(method))?
}

/// A live `events.subscribe` connection delivering ndjson event lines.
///
/// herdr quirks the consumer must handle: retained lifecycle events
/// (`pane_closed`/`pane_exited`) are replayed at subscribe time and the
/// per-pane filter is not reliably applied to them, so filter by
/// `data.pane_id` and re-verify close events with `pane.get`. A subscribed
/// connection serves no further requests.
pub(crate) struct EventStream {
    lines: Lines<BufReader<UnixStream>>,
}

/// Subscribe to agent-status and lifecycle events for one pane.
pub(crate) async fn subscribe_pane_events(socket: &Path, pane_id: &str) -> MuxResult<EventStream> {
    let fut = async {
        let mut stream = UnixStream::connect(socket).await?;
        let line = serde_json::to_string(&json!({
            "id": "meguri:events.subscribe",
            "method": "events.subscribe",
            "params": { "subscriptions": [
                { "type": "pane.agent_status_changed", "pane_id": pane_id },
                { "type": "pane.closed", "pane_id": pane_id },
                { "type": "pane.exited", "pane_id": pane_id },
            ]},
        }))
        .expect("request serializes");
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        let mut lines = BufReader::new(stream).lines();
        let first = lines
            .next_line()
            .await?
            .ok_or_else(|| eof_error("events.subscribe"))?;
        parse_response(first.trim(), "events.subscribe")?;
        Ok(EventStream { lines })
    };
    tokio::time::timeout(REQUEST_TIMEOUT, fut)
        .await
        .map_err(|_| timeout_error("events.subscribe"))?
}

impl EventStream {
    /// Next `{event, data}` line, skipping anything else. `None` on EOF.
    pub(crate) async fn next_event(&mut self) -> MuxResult<Option<(String, Value)>> {
        while let Some(line) = self.lines.next_line().await? {
            let Ok(parsed) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let (Some(event), Some(data)) = (
                parsed.get("event").and_then(Value::as_str),
                parsed.get("data"),
            ) {
                return Ok(Some((event.to_string(), data.clone())));
            }
        }
        Ok(None)
    }
}

fn parse_response(raw: &str, method: &str) -> MuxResult<Value> {
    // Garbage means a protocol mismatch, not a herdr answer: report it as Io
    // so callers route to the CLI.
    let parsed: Value = serde_json::from_str(raw).map_err(|e| {
        MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unparseable herdr socket response for {method}: {e}: {raw}"),
        ))
    })?;
    if let Some(msg) = parsed.pointer("/error/message").and_then(Value::as_str) {
        return Err(MuxError::CommandFailed {
            kind: "herdr",
            detail: msg.to_string(),
        });
    }
    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

fn eof_error(method: &str) -> MuxError {
    MuxError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("herdr socket closed before responding to {method}"),
    ))
}

fn timeout_error(method: &str) -> MuxError {
    MuxError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("herdr socket did not answer {method} within {REQUEST_TIMEOUT:?}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_returns_result() {
        let raw = r#"{"id":"x","result":{"type":"pane_info","pane":{"agent_status":"idle"}}}"#;
        let result = parse_response(raw, "pane.get").unwrap();
        assert_eq!(
            result.pointer("/pane/agent_status").and_then(Value::as_str),
            Some("idle")
        );
    }

    #[test]
    fn parse_response_maps_server_errors_to_command_failed() {
        let raw =
            r#"{"id":"x","error":{"code":"pane_not_found","message":"pane w9:p9 not found"}}"#;
        match parse_response(raw, "pane.get") {
            Err(MuxError::CommandFailed { detail, .. }) => {
                assert_eq!(detail, "pane w9:p9 not found");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_maps_garbage_to_io() {
        assert!(matches!(
            parse_response("not json", "pane.get"),
            Err(MuxError::Io(_))
        ));
    }
}
