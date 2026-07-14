//! The notify sink (issue #7, generalized in #205): push selected events to a
//! human — macOS notification + webhook — instead of leaving them buried in
//! the event log.
//!
//! Sources emit their events as usual and then hand a [`Notification`] to the
//! [`Notifier`]; the allowlist decision (which `events` tokens are delivered),
//! per-key throttling, and the per-webhook-flavor payload shaping all live
//! here, in one place, so no source re-implements them.
//!
//! Delivery lives behind the [`NotifyGateway`] trait (faked in tests). The
//! production [`SystemGateway`] shells out to `osascript` and `curl` — the
//! codebase embeds no HTTP client (GitHub goes through the `gh` CLI too).
//! Every channel is best-effort: a delivery failure is logged, never fails a
//! turn (issue #205 invariant, ADR 0018).

pub mod fake;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::json;

use crate::config::{NotificationsConfig, WebhookKind};

/// The public `events` allowlist tokens (config-facing names). One token can
/// bundle several internal store event kinds — notably `awaiting_human` covers
/// `turn.awaiting_human` / `review.awaiting_human` / `spec_fixer.budget_exhausted`
/// (ADR 0018). `label` is intentionally absent: label watching is authorized
/// per-project via `[projects.notify]`, not through this global list.
pub const NOTIFY_EVENT_TOKENS: &[&str] = &[
    "awaiting_human",
    "escalation",
    "schedule.failed",
    "schedule.skipped",
];

/// One notification ready for delivery. Sources build these through the
/// constructors below; the gateway renders them per channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// The config `events` token this belongs to (allowlist key). The literal
    /// `"label"` bypasses the global allowlist — it is authorized per-project.
    pub event: String,
    /// Throttle/dedup key: per-run, per-schedule, or per-issue. Repeated
    /// notifications with the same key inside the throttle window are dropped.
    pub dedup_key: String,
    /// Short title line (macOS title / structured payload `title`).
    pub title: String,
    /// One-line human body (macOS body / Slack & ntfy text / payload `text`).
    pub body: String,
    /// A URL the human should open (PR / issue), when there is one.
    pub url: Option<String>,
}

impl Notification {
    /// A "page a human now" notification — the three awaiting_human paths
    /// (live pane, parked review, spec-fixer budget). `attach` is the launch
    /// -mode attach hint (pane case); `url` points at a PR (parked case). When
    /// neither is set the body falls back to `meguri attach <dedup_key>`.
    pub fn awaiting_human(
        dedup_key: String,
        issue_number: i64,
        issue_title: Option<String>,
        reason: &str,
        attach: Option<String>,
        url: Option<String>,
    ) -> Self {
        let title = format!(
            "meguri #{issue_number} {}",
            issue_title.as_deref().unwrap_or("")
        )
        .trim()
        .to_string();
        let target = url
            .clone()
            .or(attach)
            .unwrap_or_else(|| format!("meguri attach {dedup_key}"));
        let body = format!("{} — {target}", reason_label(reason));
        Self {
            event: "awaiting_human".into(),
            dedup_key,
            title,
            body,
            url,
        }
    }

    /// An issue/local task escalated to `meguri:needs-human` (via
    /// `escalation.rs`). `target` is `"issue"` or `"local"`.
    pub fn escalation_task(id: i64, target: &str, reason: &str) -> Self {
        Self {
            event: "escalation".into(),
            dedup_key: format!("{target}:{id}"),
            title: format!("meguri #{id} → needs-human"),
            body: format!("#{id} ({target}) を needs-human にしました: {reason}"),
            url: None,
        }
    }

    /// A pull request parked on `meguri:needs-human` (via `escalation.rs`).
    pub fn escalation_pr(pr: i64) -> Self {
        Self {
            event: "escalation".into(),
            dedup_key: format!("pr:{pr}"),
            title: format!("meguri PR #{pr} → needs-human"),
            body: format!("PR #{pr} を needs-human にしました(人間のレビュー待ち)"),
            url: None,
        }
    }

    /// A schedule that failed to fire (the `sweep` Err arm, issue #205).
    pub fn schedule_failed(project: &str, schedule: &str, error: &str) -> Self {
        Self {
            event: "schedule.failed".into(),
            dedup_key: format!("schedule:{project}:{schedule}"),
            title: format!("meguri schedule {schedule} 失敗"),
            body: format!("schedule \"{schedule}\" ({project}) の発火に失敗しました: {error}"),
            url: None,
        }
    }

    /// A schedule occurrence skipped by the overlap guard (`scheduler_fire.rs`).
    pub fn schedule_skipped(project: &str, schedule: &str, open_key: i64) -> Self {
        Self {
            event: "schedule.skipped".into(),
            dedup_key: format!("schedule:{project}:{schedule}"),
            title: format!("meguri schedule {schedule} スキップ"),
            body: format!(
                "schedule \"{schedule}\" ({project}) を overlap でスキップ(#{open_key} が open)"
            ),
            url: None,
        }
    }

    /// A meguri-created issue carrying a watched label (per-project
    /// `[projects.notify]`, issue #205). Bypasses the global allowlist.
    pub fn label(issue_number: i64, issue_title: &str, label: &str, url: Option<String>) -> Self {
        Self {
            event: "label".into(),
            dedup_key: format!("issue:{issue_number}"),
            title: format!("meguri #{issue_number} {issue_title}")
                .trim()
                .to_string(),
            body: format!("#{issue_number}「{issue_title}」に {label} が付きました"),
            url,
        }
    }
}

/// Delivery-only gateway; policy (allowlist + throttling) lives in [`Notifier`].
/// Implemented over osascript + webhook in production; faked in tests.
#[async_trait]
pub trait NotifyGateway: Send + Sync {
    async fn deliver(&self, n: &Notification);
}

/// Allowlist + per-key throttling over a gateway. A notification whose event
/// token is not enabled is dropped; one whose dedup key was delivered less than
/// `throttle` ago is dropped. Throttled attempts do not extend the window — it
/// is anchored to deliveries.
pub struct Notifier {
    gateway: Arc<dyn NotifyGateway>,
    throttle: Duration,
    /// Enabled `events` tokens. `label` is always allowed (per-project gated).
    events: Vec<String>,
    last_delivered: Mutex<HashMap<String, tokio::time::Instant>>,
}

impl Notifier {
    pub fn new(gateway: Arc<dyn NotifyGateway>, throttle: Duration, events: Vec<String>) -> Self {
        Self {
            gateway,
            throttle,
            events,
            last_delivered: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_config(cfg: &NotificationsConfig) -> Self {
        Self::new(
            Arc::new(SystemGateway::new(cfg.clone())),
            Duration::from_secs(cfg.throttle_secs),
            cfg.events.clone(),
        )
    }

    /// Whether this event token is delivered. `label` bypasses the global list
    /// (authorized at the call site by `[projects.notify]`).
    fn allowed(&self, event: &str) -> bool {
        event == "label" || self.events.iter().any(|e| e == event)
    }

    /// Deliver unless the event is not allowlisted, or the same dedup key was
    /// notified less than `throttle` ago. Returns whether the gateway was
    /// invoked.
    pub async fn notify(&self, n: &Notification) -> bool {
        if !self.allowed(&n.event) {
            tracing::debug!(event = %n.event, "notification event not in allowlist");
            return false;
        }
        {
            let mut last = self.last_delivered.lock().unwrap();
            let now = tokio::time::Instant::now();
            let throttled = last
                .get(&n.dedup_key)
                .is_some_and(|prev| now.duration_since(*prev) < self.throttle);
            if throttled {
                tracing::debug!(key = %n.dedup_key, event = %n.event, "notification throttled");
                return false;
            }
            last.insert(n.dedup_key.clone(), now);
        }
        self.gateway.deliver(n).await;
        true
    }

    /// Page each watched label the newly-created issue carries — the shared
    /// hook every meguri issue-creation site routes through (issue #205). A
    /// `label` notification bypasses the global `events` allowlist (it is
    /// authorized per-project). Best-effort and cheap when nothing is watched.
    pub async fn notify_labels(
        &self,
        number: i64,
        title: &str,
        watched: &[String],
        labels: &[&str],
    ) {
        if watched.is_empty() {
            return;
        }
        for label in labels {
            if watched.iter().any(|w| w == label) {
                self.notify(&Notification::label(number, title, label, None))
                    .await;
            }
        }
    }
}

/// Production gateway: macOS notification via `osascript`, webhook via `curl`
/// (the codebase shells out to CLIs rather than embedding clients). Both
/// channels are best-effort — failures are logged, never fail the turn.
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
            let kind = resolve_kind(&self.cfg, url);
            if let Err(err) = post_webhook(url, kind, n).await {
                // Log the host, never the full URL: a Slack/ntfy webhook URL
                // carries a secret token in its path (issue #205).
                tracing::warn!(host = webhook_host(url), ?kind, %err, "webhook POST failed");
            }
        }
    }
}

/// The host portion of a webhook URL — the non-secret part safe to log. A
/// webhook URL's *path* carries the token (Slack `.../services/T/B/XXXX`), so
/// only the host is logged on failure.
fn webhook_host(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
}

/// Resolve the webhook flavor: the explicit `kind`, else auto-detect from the
/// URL host.
pub fn resolve_kind(cfg: &NotificationsConfig, url: &str) -> WebhookKind {
    cfg.kind.unwrap_or_else(|| detect_kind(url))
}

/// Auto-detect the webhook flavor from the URL. Slack Incoming Webhooks live on
/// `hooks.slack.com`; ntfy on `ntfy.sh` or a `/ntfy` path; everything else gets
/// the generic structured JSON.
fn detect_kind(url: &str) -> WebhookKind {
    if url.contains("hooks.slack.com") {
        WebhookKind::Slack
    } else if url.contains("ntfy.sh") || url.contains("/ntfy") {
        WebhookKind::Ntfy
    } else {
        WebhookKind::Json
    }
}

/// One webhook request: HTTP headers plus the raw POST body.
struct WebhookRequest {
    headers: Vec<(&'static str, String)>,
    body: String,
}

/// Shape the request body for one webhook flavor (issue #205). Slack wants
/// `{"text": ...}`, ntfy takes the plain line as the body (title/click as
/// headers), `json` gets the structured payload.
fn webhook_request(kind: WebhookKind, n: &Notification) -> WebhookRequest {
    match kind {
        WebhookKind::Slack => {
            let text = match &n.url {
                Some(u) => format!("{} — {}\n{u}", n.title, n.body),
                None => format!("{} — {}", n.title, n.body),
            };
            WebhookRequest {
                headers: vec![("Content-Type", "application/json".into())],
                body: json!({ "text": text }).to_string(),
            }
        }
        WebhookKind::Ntfy => {
            let mut headers = vec![("Title", n.title.clone())];
            if let Some(u) = &n.url {
                headers.push(("Click", u.clone()));
            }
            WebhookRequest {
                headers,
                body: n.body.clone(),
            }
        }
        WebhookKind::Json => WebhookRequest {
            headers: vec![("Content-Type", "application/json".into())],
            body: json!({
                "event": n.event,
                "title": n.title,
                "text": n.body,
                "url": n.url,
            })
            .to_string(),
        },
    }
}

/// POST one notification to `url` via `curl`. Propagates the error so the
/// doctor probe can report it; the gateway swallows it (best-effort).
async fn post_webhook(url: &str, kind: WebhookKind, n: &Notification) -> Result<()> {
    let req = webhook_request(kind, n);
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["-fsS", "--max-time", "10", "-X", "POST"]);
    for (k, v) in &req.headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    cmd.arg("--data").arg(&req.body).arg(url);
    let out = cmd.output().await.context("cannot spawn curl")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "curl exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Send a test notification to the configured webhook (doctor `--probe`). Uses
/// the same code path as real delivery, but surfaces the error.
pub async fn probe_webhook(cfg: &NotificationsConfig, url: &str) -> Result<()> {
    let n = Notification {
        event: "awaiting_human".into(),
        dedup_key: "doctor-probe".into(),
        title: "meguri doctor".into(),
        body: "通知シンクのテスト送信です".into(),
        url: None,
    };
    post_webhook(url, resolve_kind(cfg, url), &n).await
}

/// `display notification` AppleScript for one notification.
fn osascript_notification(n: &Notification) -> String {
    format!(
        "display notification {} with title {}",
        applescript_str(n.body.trim()),
        applescript_str(n.title.trim())
    )
}

/// Human-readable label for an awaiting_human reason.
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

#[cfg(test)]
mod tests {
    use super::fake::FakeGateway;
    use super::*;

    fn awaiting(run: &str) -> Notification {
        Notification::awaiting_human(
            run.into(),
            7,
            Some("awaiting_human 通知".into()),
            "agent_blocked",
            Some("tmux attach -t meguri".into()),
            None,
        )
    }

    fn permissive(gw: Arc<FakeGateway>, throttle_secs: u64) -> Notifier {
        Notifier::new(
            gw,
            Duration::from_secs(throttle_secs),
            NOTIFY_EVENT_TOKENS.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn second_notification_inside_window_is_throttled() {
        let gw = Arc::new(FakeGateway::default());
        let notifier = permissive(gw.clone(), 60);
        assert!(notifier.notify(&awaiting("r1")).await);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!notifier.notify(&awaiting("r1")).await);
        assert_eq!(gw.delivered().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn boundary_exactly_at_window_delivers() {
        let gw = Arc::new(FakeGateway::default());
        let notifier = permissive(gw.clone(), 60);
        assert!(notifier.notify(&awaiting("r1")).await);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!notifier.notify(&awaiting("r1")).await);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(notifier.notify(&awaiting("r1")).await);
        assert_eq!(gw.delivered().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn distinct_keys_do_not_throttle_each_other() {
        let gw = Arc::new(FakeGateway::default());
        let notifier = permissive(gw.clone(), 60);
        assert!(notifier.notify(&awaiting("r1")).await);
        assert!(notifier.notify(&awaiting("r2")).await);
        assert_eq!(gw.delivered().len(), 2);
    }

    #[tokio::test]
    async fn event_not_in_allowlist_is_dropped() {
        let gw = Arc::new(FakeGateway::default());
        // Only awaiting_human enabled — an escalation must not deliver.
        let notifier = Notifier::new(
            gw.clone(),
            Duration::from_secs(60),
            vec!["awaiting_human".into()],
        );
        assert!(
            !notifier
                .notify(&Notification::escalation_task(7, "issue", "ci red"))
                .await
        );
        assert!(notifier.notify(&awaiting("r1")).await);
        assert_eq!(gw.delivered().len(), 1);
    }

    #[tokio::test]
    async fn label_bypasses_the_global_allowlist() {
        let gw = Arc::new(FakeGateway::default());
        // Empty allowlist: label still delivers (per-project authorized).
        let notifier = Notifier::new(gw.clone(), Duration::from_secs(60), vec![]);
        assert!(
            notifier
                .notify(&Notification::label(9, "human task", "human:todo", None))
                .await
        );
        assert_eq!(gw.delivered().len(), 1);
    }

    #[test]
    fn slack_payload_is_a_text_object() {
        let n = Notification::escalation_pr(12);
        let req = webhook_request(WebhookKind::Slack, &n);
        let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert!(v["text"].as_str().unwrap().contains("PR #12"));
    }

    #[test]
    fn json_payload_carries_event_title_text() {
        let n = Notification::schedule_failed("proj", "nightly", "boom");
        let req = webhook_request(WebhookKind::Json, &n);
        let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(v["event"], "schedule.failed");
        assert!(v["text"].as_str().unwrap().contains("boom"));
        assert!(v["url"].is_null());
    }

    #[test]
    fn ntfy_body_is_plain_text_with_title_header() {
        let n = Notification::awaiting_human("r1".into(), 7, None, "agent_blocked", None, None);
        let req = webhook_request(WebhookKind::Ntfy, &n);
        assert!(req.body.contains("エージェントが人の入力を待っています"));
        assert!(req.headers.iter().any(|(k, _)| *k == "Title"));
    }

    #[test]
    fn webhook_host_is_the_non_secret_part() {
        // The token lives in the path; only the host may be logged.
        assert_eq!(
            webhook_host("https://hooks.slack.com/services/T0/B0/secrettoken"),
            "hooks.slack.com"
        );
        assert_eq!(webhook_host("https://ntfy.sh/mytopic?x=1"), "ntfy.sh");
        assert_eq!(webhook_host("weird"), "weird");
    }

    #[test]
    fn kind_auto_detects_from_url() {
        assert_eq!(
            detect_kind("https://hooks.slack.com/services/x"),
            WebhookKind::Slack
        );
        assert_eq!(detect_kind("https://ntfy.sh/mytopic"), WebhookKind::Ntfy);
        assert_eq!(detect_kind("https://example.com/hook"), WebhookKind::Json);
    }

    #[test]
    fn parked_awaiting_human_points_at_pr() {
        let n = Notification::awaiting_human(
            "run-9".into(),
            7,
            Some("Spec: caching (#7)".into()),
            "spec_review_parked",
            None,
            Some("https://example.test/pr/12".into()),
        );
        assert_eq!(n.url.as_deref(), Some("https://example.test/pr/12"));
        assert!(n.body.contains("https://example.test/pr/12"));
        assert!(!n.body.contains("meguri attach"));
    }

    #[test]
    fn osascript_escapes_quotes_and_backslashes() {
        let mut n = awaiting("run-1");
        n.title = r#"say "hi" \ bye"#.into();
        let script = osascript_notification(&n);
        assert!(script.contains(r#"say \"hi\" \\ bye"#), "got: {script}");
        assert!(script.starts_with("display notification \""));
    }
}
