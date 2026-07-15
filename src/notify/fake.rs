//! Recording-only `NotifyGateway` used by tests: delivers nothing, keeps
//! every notification for assertions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::{NOTIFY_EVENT_TOKENS, Notification, Notifier, NotifyGateway};

#[derive(Default)]
pub struct FakeGateway {
    pub delivered: Mutex<Vec<Notification>>,
}

impl FakeGateway {
    pub fn delivered(&self) -> Vec<Notification> {
        self.delivered.lock().unwrap().clone()
    }
}

#[async_trait]
impl NotifyGateway for FakeGateway {
    async fn deliver(&self, n: &Notification) {
        self.delivered.lock().unwrap().push(n.clone());
    }
}

/// Notifier over a fresh recording fake (default 60s throttle) with every
/// event token enabled, so a test sees deliveries unless it is specifically
/// exercising the allowlist. Returns both so tests can assert on deliveries;
/// tests that don't care take `.0`.
pub fn recording_notifier() -> (Arc<Notifier>, Arc<FakeGateway>) {
    recording_notifier_with_events(NOTIFY_EVENT_TOKENS)
}

/// Like [`recording_notifier`] but with an explicit allowlist — for tests that
/// assert an event is dropped when its token is absent.
pub fn recording_notifier_with_events(events: &[&str]) -> (Arc<Notifier>, Arc<FakeGateway>) {
    let gw = Arc::new(FakeGateway::default());
    let notifier = Arc::new(Notifier::new(
        gw.clone(),
        Duration::from_secs(60),
        events.iter().map(|s| s.to_string()).collect(),
    ));
    (notifier, gw)
}
