//! awaiting_human notifications: page a human (macOS notification + webhook)
//! when a run needs attention, instead of leaving it buried in the event log.
//!
//! Delivery lives behind the `NotifyGateway` trait (faked in tests); the
//! `Notifier` wraps a gateway with per-run throttling so repeated
//! escalations of the same run don't spam the human.

pub mod fake;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::config::NotificationsConfig;

/// One awaiting_human escalation, ready for delivery.
#[derive(Debug, Clone)]
pub struct Notification {
    pub run_id: String,
    pub issue_number: i64,
    pub issue_title: Option<String>,
    /// "agent_blocked" | "agent_quiet" | "runtime_budget_exceeded" |
    /// "spec_review_parked"
    pub reason: String,
    /// Shell command a human runs to attach to the live pane, or `None` when
    /// the wait has no pane (a parked review — the run already finished, so
    /// the pointer is the PR, not a pane). ADR 0009 / issue #153.
    pub attach: Option<String>,
    /// Web page a human should open instead of attaching — the PR for a
    /// parked review. `None` for the turn-scoped escalations (they point at a
    /// pane via `attach`).
    pub url: Option<String>,
}

/// Delivery-only gateway; policy (throttling) lives in `Notifier`.
/// Implemented over osascript + webhook in production; faked in tests.
#[async_trait]
pub trait NotifyGateway: Send + Sync {
    async fn deliver(&self, n: &Notification);
}

/// Per-run throttling over a gateway: a notification for a run that was
/// already delivered within the throttle window is dropped. Throttled
/// attempts do not extend the window — it is anchored to deliveries.
pub struct Notifier {
    gateway: Arc<dyn NotifyGateway>,
    throttle: Duration,
    last_delivered: Mutex<HashMap<String, tokio::time::Instant>>,
}

impl Notifier {
    pub fn new(gateway: Arc<dyn NotifyGateway>, throttle: Duration) -> Self {
        Self {
            gateway,
            throttle,
            last_delivered: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_config(cfg: &NotificationsConfig) -> Self {
        Self::new(
            Arc::new(SystemGateway::new(cfg.clone())),
            Duration::from_secs(cfg.throttle_secs),
        )
    }

    /// Deliver unless the same run was notified less than `throttle` ago.
    /// Returns whether the gateway was invoked.
    pub async fn notify_awaiting_human(&self, n: &Notification) -> bool {
        {
            let mut last = self.last_delivered.lock().unwrap();
            let now = tokio::time::Instant::now();
            let throttled = last
                .get(&n.run_id)
                .is_some_and(|prev| now.duration_since(*prev) < self.throttle);
            if throttled {
                tracing::debug!(run_id = %n.run_id, reason = %n.reason, "notification throttled");
                return false;
            }
            last.insert(n.run_id.clone(), now);
        }
        self.gateway.deliver(n).await;
        true
    }
}

/// Production gateway: macOS notification via `osascript`, webhook via
/// `curl` (the codebase shells out to CLIs rather than embedding clients).
/// Both channels are best-effort — failures are logged, never fail the turn.
pub struct SystemGateway {
    cfg: NotificationsConfig,
}

impl SystemGateway {
    pub fn new(cfg: NotificationsConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl NotifyGateway for SystemGateway {
    async fn deliver(&self, n: &Notification) {
        if self.cfg.macos && cfg!(target_os = "macos") {
            let script = osascript_notification(n);
            match tokio::process::Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .output()
                .await
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => tracing::warn!(
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "osascript notification failed"
                ),
                Err(err) => tracing::warn!(%err, "cannot spawn osascript"),
            }
        }
        if let Some(url) = &self.cfg.webhook_url {
            let payload = webhook_payload(n).to_string();
            match tokio::process::Command::new("curl")
                .args(["-fsS", "--max-time", "10", "-X", "POST"])
                .args(["-H", "Content-Type: application/json"])
                .args(["--data", &payload])
                .arg(url)
                .output()
                .await
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => tracing::warn!(
                    url,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "webhook POST failed"
                ),
                Err(err) => tracing::warn!(url, %err, "cannot spawn curl"),
            }
        }
    }
}

/// `display notification` AppleScript for one escalation.
fn osascript_notification(n: &Notification) -> String {
    let title = format!(
        "meguri #{} {}",
        n.issue_number,
        n.issue_title.as_deref().unwrap_or("")
    );
    // Point at the PR when there is one (parked review), else the pane.
    let target = match &n.url {
        Some(url) => url.clone(),
        None => format!("meguri attach {}", n.run_id),
    };
    let body = format!("{} — {target}", reason_label(&n.reason));
    format!(
        "display notification {} with title {}",
        applescript_str(body.trim()),
        applescript_str(title.trim())
    )
}

/// Human-readable label for the escalation reason.
fn reason_label(reason: &str) -> &str {
    match reason {
        "agent_blocked" => "エージェントが人の入力を待っています",
        "agent_quiet" => "エージェントが沈黙しています",
        "runtime_budget_exceeded" => "turn の実行時間が上限を超えました",
        "spec_review_parked" => "spec レビューが人間の判断待ちです",
        other => other,
    }
}

/// Quote a string as an AppleScript string literal.
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// JSON body POSTed to the webhook.
fn webhook_payload(n: &Notification) -> serde_json::Value {
    let mut payload = json!({
        "event": "turn.awaiting_human",
        "run_id": n.run_id,
        "issue_number": n.issue_number,
        "issue_title": n.issue_title,
        "reason": n.reason,
        "attach": n.attach,
        "url": n.url,
    });
    // Only advertise `meguri attach` when there is a live pane to attach to —
    // a finished (parked) run has none, so the CLI hint would be a dead end.
    if n.attach.is_some() {
        payload["attach_cli"] = json!(format!("meguri attach {}", n.run_id));
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::fake::FakeGateway;
    use super::*;

    fn n(run: &str) -> Notification {
        Notification {
            run_id: run.into(),
            issue_number: 7,
            issue_title: Some("awaiting_human 通知".into()),
            reason: "agent_blocked".into(),
            attach: Some("tmux attach -t meguri".into()),
            url: None,
        }
    }

    /// A parked-review notification: no pane, points at the PR (ADR 0009).
    fn parked(run: &str) -> Notification {
        Notification {
            run_id: run.into(),
            issue_number: 7,
            issue_title: Some("Spec: caching (#7)".into()),
            reason: "spec_review_parked".into(),
            attach: None,
            url: Some("https://example.test/pr/12".into()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn second_notification_inside_window_is_throttled() {
        let gw = Arc::new(FakeGateway::default());
        let notifier = Notifier::new(gw.clone(), Duration::from_secs(60));
        assert!(notifier.notify_awaiting_human(&n("r1")).await);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!notifier.notify_awaiting_human(&n("r1")).await);
        assert_eq!(gw.delivered().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn boundary_exactly_at_window_delivers() {
        let gw = Arc::new(FakeGateway::default());
        let notifier = Notifier::new(gw.clone(), Duration::from_secs(60));
        assert!(notifier.notify_awaiting_human(&n("r1")).await);
        // Throttled attempt at 59s must NOT extend the window…
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!notifier.notify_awaiting_human(&n("r1")).await);
        // …so exactly 60s after the delivery, the next one goes out.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(notifier.notify_awaiting_human(&n("r1")).await);
        assert_eq!(gw.delivered().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn distinct_runs_do_not_throttle_each_other() {
        let gw = Arc::new(FakeGateway::default());
        let notifier = Notifier::new(gw.clone(), Duration::from_secs(60));
        assert!(notifier.notify_awaiting_human(&n("r1")).await);
        assert!(notifier.notify_awaiting_human(&n("r2")).await);
        assert_eq!(gw.delivered().len(), 2);
    }

    #[test]
    fn webhook_payload_carries_run_issue_reason_attach() {
        let p = webhook_payload(&n("run-1"));
        assert_eq!(p["event"], "turn.awaiting_human");
        assert_eq!(p["run_id"], "run-1");
        assert_eq!(p["issue_number"], 7);
        assert_eq!(p["issue_title"], "awaiting_human 通知");
        assert_eq!(p["reason"], "agent_blocked");
        assert_eq!(p["attach"], "tmux attach -t meguri");
        assert_eq!(p["attach_cli"], "meguri attach run-1");
        assert!(p["url"].is_null());
    }

    #[test]
    fn parked_payload_points_at_pr_not_pane() {
        // A parked review has no pane: the webhook carries `url` and omits the
        // `meguri attach` dead end (ADR 0009 / issue #153).
        let p = webhook_payload(&parked("run-9"));
        assert_eq!(p["reason"], "spec_review_parked");
        assert_eq!(p["url"], "https://example.test/pr/12");
        assert!(p["attach"].is_null());
        assert!(p.get("attach_cli").is_none());
    }

    #[test]
    fn parked_osascript_body_shows_the_pr_url() {
        let script = osascript_notification(&parked("run-9"));
        assert!(
            script.contains("https://example.test/pr/12"),
            "got: {script}"
        );
        assert!(!script.contains("meguri attach"), "got: {script}");
    }

    #[test]
    fn osascript_escapes_quotes_and_backslashes() {
        let mut notif = n("run-1");
        notif.issue_title = Some(r#"say "hi" \ bye"#.into());
        let script = osascript_notification(&notif);
        assert!(script.contains(r#"say \"hi\" \\ bye"#), "got: {script}");
        assert!(script.starts_with("display notification \""));
    }
}
