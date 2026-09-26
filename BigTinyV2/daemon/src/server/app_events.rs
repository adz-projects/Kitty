//! The per-app event hub behind `GET /api/apps/me/events`.
//!
//! A turn's own SSE stream reaches only whoever sent that turn's message. Some
//! events matter to an app even when no stream of its own is open: an approval
//! a background chat or a scheduled run is waiting on, or a scheduled run
//! finishing. Those are published here as well, tagged with the owning app,
//! and each app's event stream forwards only its own.
//!
//! A `broadcast` channel rather than per-app channels: publishers never need
//! to know who is listening (usually one subscriber, often none), and an event
//! nobody is subscribed to is simply dropped, which is correct -- a client
//! that attaches later recovers the state it missed from `GET .../pending` or
//! the schedule's run history rather than from a backlog.

use tokio::sync::broadcast;

use super::events::SSEEvent;

/// Enough to ride out a burst (every tool call of a fan-out pausing at once)
/// without a slow subscriber lagging. A subscriber that does lag is told so
/// and re-syncs from the REST endpoints.
const CAPACITY: usize = 256;

pub struct AppEvents {
    tx: broadcast::Sender<(String, SSEEvent)>,
}

impl Default for AppEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl AppEvents {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    /// Publish `event` to `app_id`'s stream. A no-op when nobody listens.
    pub fn publish(&self, app_id: &str, event: SSEEvent) {
        let _ = self.tx.send((app_id.to_string(), event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(String, SSEEvent)> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::events::SSEEventType;

    #[tokio::test]
    async fn a_subscriber_sees_events_for_every_app_tagged_with_the_owner() {
        let hub = AppEvents::new();
        let mut rx = hub.subscribe();
        hub.publish("a", SSEEvent { event_type: SSEEventType::HitlPause, ..Default::default() });
        hub.publish("b", SSEEvent { event_type: SSEEventType::ScheduleRun, ..Default::default() });
        assert_eq!(rx.recv().await.unwrap().0, "a");
        assert_eq!(rx.recv().await.unwrap().0, "b");
    }

    #[test]
    fn publishing_with_no_subscriber_is_harmless() {
        AppEvents::new().publish("a", SSEEvent::content("x"));
    }
}
