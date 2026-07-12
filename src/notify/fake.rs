//! Recording-only `NotifyGateway` used by tests: delivers nothing, keeps
//! every notification for assertions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::{Notification, Notifier, NotifyGateway};

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

/// Notifier over a fresh recording fake (default 60s throttle); returns both
/// so tests can assert on deliveries. Tests that don't care take `.0`.
pub fn recording_notifier() -> (Arc<Notifier>, Arc<FakeGateway>) {
    let gw = Arc::new(FakeGateway::default());
    let notifier = Arc::new(Notifier::new(gw.clone(), Duration::from_secs(60)));
    (notifier, gw)
}
