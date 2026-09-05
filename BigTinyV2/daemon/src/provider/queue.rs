//! Fair, per-app admission to a provider's concurrency slots.
//!
//! # What this replaces
//!
//! V1 gated each provider with a plain `Semaphore`. That is exactly right for
//! one client: requests queue FIFO and everybody gets served. With several
//! apps it is a starvation bug. A `custom_openai` endpoint defaults to **one**
//! slot (a llama-server built without `--parallel` genuinely serves one request
//! at a time), so a pipeline that submits fifty turns puts fifty entries in
//! front of the next interactive message. The chat window then waits for fifty
//! generations with no explanation.
//!
//! # The rules
//!
//! 1. **Round-robin across apps.** On release, the next permit goes to the next
//!    app with someone waiting, not to whoever queued first.
//! 2. **Interactive beats background, within an app.** Compaction, title
//!    derivation and the pathway learn pass are `Background`; a user's turn is
//!    `Interactive`. This is what replaced V1's habit of *aborting* another
//!    session's turn-end work to get the endpoint back (see
//!    `AgentLoop::track_background`) -- the work now waits instead of dying.
//! 3. **Work-conserving.** The fair share binds only while another app is
//!    actually waiting. One app alone may hold every slot.
//!
//! Rule 3 is the one that is easy to get wrong, and getting it wrong breaks a
//! requirement rather than merely being suboptimal: an app must be able to fan
//! out across all available slots (subagents, a batch job) when nothing else
//! wants them. A cap that applied unconditionally would make the fix for
//! cross-app starvation into a cause of single-app underuse.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex};

/// The lane used by daemon-internal work that cannot name an app.
///
/// `SummarizerChain` implements `adaptive_pathway::traits::StructuredChat`,
/// whose signature is fixed and carries no identity, so compaction and the
/// learn pass cannot say whose turn they are serving. They queue here instead:
/// one lane competing round-robin against the real apps, always at
/// `Background` priority.
///
/// The property that matters is preserved either way -- a user's interactive
/// turn is in its own app's lane and is never stuck behind this one for longer
/// than a single release.
pub const DAEMON_LANE: &str = "__daemon__";

/// Why a request wants the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// A user is waiting on this.
    Interactive,
    /// Turn-end work: compaction, title derivation, pathway learning, and
    /// detached jobs. Yields to interactive traffic from the same app.
    Background,
}

struct Waiter {
    priority: Priority,
    tx: oneshot::Sender<()>,
}

#[derive(Default)]
struct AppQueue {
    interactive: VecDeque<Waiter>,
    background: VecDeque<Waiter>,
    /// Permits this app currently holds. Drives the fair-share ceiling.
    in_flight: u32,
}

impl AppQueue {
    fn is_waiting(&self) -> bool {
        !self.interactive.is_empty() || !self.background.is_empty()
    }

    fn pop(&mut self) -> Option<Waiter> {
        self.interactive
            .pop_front()
            .or_else(|| self.background.pop_front())
    }
}

struct Inner {
    limit: u32,
    in_flight: u32,
    /// Insertion-ordered so the round-robin cursor is stable across ticks;
    /// a `HashMap` alone would make "the next app" depend on hash order.
    apps: HashMap<String, AppQueue>,
    order: Vec<String>,
    cursor: usize,
}

impl Inner {
    /// Apps either holding a permit or waiting for one — the denominator of
    /// the fair share.
    fn active_apps(&self) -> u32 {
        self.apps
            .values()
            .filter(|q| q.in_flight > 0 || q.is_waiting())
            .count()
            .max(1) as u32
    }

    /// Whether `app_id` may take a slot right now.
    ///
    /// The work-conserving rule lives here: with nobody else waiting, the
    /// share does not apply at all.
    fn may_admit(&self, app_id: &str) -> bool {
        if self.in_flight >= self.limit {
            return false;
        }
        let others_waiting = self
            .apps
            .iter()
            .any(|(id, q)| id != app_id && q.is_waiting());
        if !others_waiting {
            return true;
        }
        let mine = self.apps.get(app_id).map(|q| q.in_flight).unwrap_or(0);
        let share = self.limit.div_ceil(self.active_apps());
        mine < share
    }

    fn queue_for(&mut self, app_id: &str) -> &mut AppQueue {
        if !self.apps.contains_key(app_id) {
            self.apps.insert(app_id.to_string(), AppQueue::default());
            self.order.push(app_id.to_string());
        }
        self.apps.get_mut(app_id).unwrap()
    }

    /// Hand the freed slot to the next app that wants it, round-robin.
    fn wake_next(&mut self) {
        if self.order.is_empty() {
            return;
        }
        for step in 0..self.order.len() {
            let idx = (self.cursor + step) % self.order.len();
            let app_id = self.order[idx].clone();

            let waiting = self.apps.get(&app_id).map(|q| q.is_waiting()).unwrap_or(false);
            if !waiting || !self.may_admit(&app_id) {
                continue;
            }
            let Some(waiter) = self.apps.get_mut(&app_id).and_then(|q| q.pop()) else {
                continue;
            };
            // Advance past the app we just served, so the next release starts
            // with someone else.
            self.cursor = (idx + 1) % self.order.len();
            self.in_flight += 1;
            if let Some(q) = self.apps.get_mut(&app_id) {
                q.in_flight += 1;
            }
            // A receiver dropped between queueing and waking (the caller gave
            // up) leaves the permit unused, so put it back and keep going.
            if waiter.tx.send(()).is_err() {
                self.in_flight -= 1;
                if let Some(q) = self.apps.get_mut(&app_id) {
                    q.in_flight = q.in_flight.saturating_sub(1);
                }
                continue;
            }
            return;
        }
    }
}

/// One provider's admission queue.
pub struct ProviderQueue {
    inner: Mutex<Inner>,
}

impl ProviderQueue {
    pub fn new(limit: u32) -> Self {
        Self {
            inner: Mutex::new(Inner {
                limit: limit.max(1),
                in_flight: 0,
                apps: HashMap::new(),
                order: Vec::new(),
                cursor: 0,
            }),
        }
    }

    pub async fn limit(&self) -> u32 {
        self.inner.lock().await.limit
    }

    /// Change the slot count in place, so editing `parallel_slots` takes
    /// effect without a daemon restart.
    ///
    /// In place rather than by swapping the queue: replacing it would strand
    /// everyone already waiting on the old one. Raising the limit immediately
    /// admits whoever fits; lowering it lets the excess drain naturally as
    /// permits are returned, rather than revoking permits already handed out.
    pub async fn set_limit(&self, limit: u32) {
        let mut inner = self.inner.lock().await;
        let limit = limit.max(1);
        if inner.limit == limit {
            return;
        }
        inner.limit = limit;
        while inner.in_flight < inner.limit {
            let before = inner.in_flight;
            inner.wake_next();
            if inner.in_flight == before {
                break; // nobody left to admit
            }
        }
    }

    pub async fn in_flight(&self) -> u32 {
        self.inner.lock().await.in_flight
    }

    /// Total waiters across every app.
    pub async fn queue_depth(&self) -> usize {
        let inner = self.inner.lock().await;
        inner
            .apps
            .values()
            .map(|q| q.interactive.len() + q.background.len())
            .sum()
    }

    /// Waiters belonging to one app.
    pub async fn queue_depth_for(&self, app_id: &str) -> usize {
        let inner = self.inner.lock().await;
        inner
            .apps
            .get(app_id)
            .map(|q| q.interactive.len() + q.background.len())
            .unwrap_or(0)
    }

    /// Take a slot, waiting if necessary.
    ///
    /// The returned guard releases on drop, so a permit cannot be leaked by an
    /// early return or a panic — which matters because the stream that holds it
    /// may end in any of several ways.
    pub async fn acquire(self: &Arc<Self>, app_id: &str, priority: Priority) -> QueuePermit {
        let rx = {
            let mut inner = self.inner.lock().await;
            if inner.may_admit(app_id) {
                inner.in_flight += 1;
                inner.queue_for(app_id).in_flight += 1;
                return QueuePermit {
                    queue: self.clone(),
                    app_id: app_id.to_string(),
                };
            }
            let (tx, rx) = oneshot::channel();
            let waiter = Waiter { priority, tx };
            let q = inner.queue_for(app_id);
            match priority {
                Priority::Interactive => q.interactive.push_back(waiter),
                Priority::Background => q.background.push_back(waiter),
            }
            rx
        };

        // `wake_next` increments the counters before signalling, so by the
        // time this resolves the slot is already ours.
        let _ = rx.await;
        QueuePermit {
            queue: self.clone(),
            app_id: app_id.to_string(),
        }
    }

    async fn release(&self, app_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.in_flight = inner.in_flight.saturating_sub(1);
        if let Some(q) = inner.apps.get_mut(app_id) {
            q.in_flight = q.in_flight.saturating_sub(1);
        }
        inner.wake_next();
    }
}

/// Holds a provider slot until dropped.
pub struct QueuePermit {
    queue: Arc<ProviderQueue>,
    app_id: String,
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        let queue = self.queue.clone();
        let app_id = std::mem::take(&mut self.app_id);
        // `Drop` cannot await. Releasing on a spawned task means the slot is
        // freed promptly without blocking whoever dropped the permit, which is
        // usually a stream being torn down.
        tokio::spawn(async move {
            queue.release(&app_id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn queue(limit: u32) -> Arc<ProviderQueue> {
        Arc::new(ProviderQueue::new(limit))
    }

    #[tokio::test]
    async fn one_app_alone_may_use_every_slot() {
        // The work-conserving rule. Without it, the fix for cross-app
        // starvation would itself stop a single app from fanning out across
        // the slots it is entitled to — breaking subagents and batch jobs.
        let q = queue(8);
        let mut permits = Vec::new();
        for _ in 0..8 {
            permits.push(
                tokio::time::timeout(Duration::from_millis(200), q.acquire("solo", Priority::Interactive))
                    .await
                    .expect("a lone app must not be throttled below the limit"),
            );
        }
        assert_eq!(q.in_flight().await, 8);
        drop(permits);
    }

    #[tokio::test]
    async fn a_full_queue_makes_the_next_caller_wait() {
        let q = queue(1);
        let _held = q.acquire("a", Priority::Interactive).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(150), q.acquire("a", Priority::Interactive))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_released_permit_admits_the_waiter() {
        let q = queue(1);
        let held = q.acquire("a", Priority::Interactive).await;

        let q2 = q.clone();
        let waiter = tokio::spawn(async move { q2.acquire("b", Priority::Interactive).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        drop(held);
        let permit = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter should be woken")
            .unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn a_batch_app_cannot_starve_an_interactive_one() {
        // The scenario this module exists for: a pipeline queues twenty turns
        // against a one-slot endpoint, then a chat window sends one message.
        //
        // Under V1's plain FIFO semaphore the chat waited for all twenty. The
        // guarantee here is round-robin, so the wait is bounded by the number
        // of *apps* rather than the queue depth: chat is served after at most
        // one pipeline turn, no matter how deep the batch is.
        let q = queue(1);
        let held = q.acquire("pipeline", Priority::Interactive).await;

        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut batch = Vec::new();
        for _ in 0..20 {
            let q2 = q.clone();
            let done = done.clone();
            batch.push(tokio::spawn(async move {
                let p = q2.acquire("pipeline", Priority::Background).await;
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(p);
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Chat arrives last, behind all twenty.
        let q3 = q.clone();
        let done_at_acquire = done.clone();
        let chat = tokio::spawn(async move {
            let p = q3.acquire("chat", Priority::Interactive).await;
            // Snapshot *at acquisition*. Reading after the task finishes would
            // be racy: dropping this permit wakes the next pipeline turn, so
            // the counter moves again before the assertion could read it.
            let seen = done_at_acquire.load(std::sync::atomic::Ordering::SeqCst);
            drop(p);
            seen
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        drop(held);
        let pipeline_turns = tokio::time::timeout(Duration::from_secs(5), chat)
            .await
            .expect("chat must not wait behind the whole batch")
            .unwrap();
        assert!(
            pipeline_turns <= 1,
            "chat waited behind {pipeline_turns} pipeline turns; round-robin bounds              the wait by the app count, not the queue depth"
        );
        for t in batch {
            t.abort();
        }
    }

    #[tokio::test]
    async fn interactive_beats_background_within_one_app() {
        let q = queue(1);
        let held = q.acquire("a", Priority::Interactive).await;

        let qb = q.clone();
        let background = tokio::spawn(async move { qb.acquire("a", Priority::Background).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let qi = q.clone();
        let interactive = tokio::spawn(async move { qi.acquire("a", Priority::Interactive).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Background queued first; interactive must still go first.
        drop(held);
        let permit = tokio::time::timeout(Duration::from_secs(2), interactive)
            .await
            .expect("interactive should preempt background")
            .unwrap();
        assert!(!background.is_finished(), "background should still be waiting");
        drop(permit);
        background.abort();
    }

    #[tokio::test]
    async fn releases_rotate_between_apps() {
        // Round-robin, not FIFO: three apps each waiting once should each be
        // served by three successive releases.
        let q = queue(1);
        let held = q.acquire("holder", Priority::Interactive).await;

        let mut handles = Vec::new();
        for app in ["a", "b", "c"] {
            let q2 = q.clone();
            let app = app.to_string();
            handles.push(tokio::spawn(async move {
                let p = q2.acquire(&app, Priority::Interactive).await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                drop(p);
                app
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(held);

        let mut served = Vec::new();
        for h in handles {
            served.push(
                tokio::time::timeout(Duration::from_secs(3), h)
                    .await
                    .expect("every app should be served")
                    .unwrap(),
            );
        }
        served.sort();
        assert_eq!(served, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn depth_reporting_distinguishes_mine_from_everyones() {
        let q = queue(1);
        let _held = q.acquire("a", Priority::Interactive).await;

        let mut tasks = Vec::new();
        for app in ["a", "b", "b"] {
            let q2 = q.clone();
            let app = app.to_string();
            tasks.push(tokio::spawn(async move {
                q2.acquire(&app, Priority::Background).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(q.queue_depth().await, 3);
        assert_eq!(q.queue_depth_for("a").await, 1);
        assert_eq!(q.queue_depth_for("b").await, 2);
        assert_eq!(q.queue_depth_for("never-seen").await, 0);
        for t in tasks {
            t.abort();
        }
    }

    #[tokio::test]
    async fn raising_the_limit_admits_waiters_without_stranding_them() {
        // Editing `parallel_slots` must take effect without a restart, and
        // must not be implemented by swapping the queue -- that would strand
        // everyone already waiting on the old one.
        let q = queue(1);
        let _held = q.acquire("a", Priority::Interactive).await;

        let q2 = q.clone();
        let waiter = tokio::spawn(async move { q2.acquire("a", Priority::Interactive).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(q.queue_depth().await, 1);

        q.set_limit(4).await;
        let permit = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("raising the limit should admit the existing waiter")
            .unwrap();
        assert_eq!(q.limit().await, 4);
        drop(permit);
    }

    #[tokio::test]
    async fn a_zero_limit_is_clamped_rather_than_deadlocking() {
        // A misconfigured `parallel_slots: 0` must not wedge the provider
        // permanently — every caller would wait forever with no diagnosis.
        let q = queue(0);
        assert_eq!(q.limit().await, 1);
        let permit = tokio::time::timeout(Duration::from_millis(200), q.acquire("a", Priority::Interactive))
            .await
            .expect("a clamped queue must still admit one");
        drop(permit);
    }
}
