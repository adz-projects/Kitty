//! Interactive embedding requests go ahead of background ones.
//!
//! Every app's memory work shares one embedder thread: the per-turn recall a
//! user is waiting on, and the ingest, learning and overnight passes nobody
//! is. In arrival order, a night-time ingest burst pushed a live recall past
//! its 1.5 s budget, and that turn quietly had no memory. The turn's recall
//! now runs inside [`interactive`], and the embedder serves those requests
//! first.
//!
//! The marker is a task-local rather than a parameter because the embedder
//! is reached through the memory plugins' `SemanticEmbedder` trait, whose
//! signature carries only the text.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Condvar, Mutex};

tokio::task_local! {
    static INTERACTIVE: bool;
}

/// Run `fut` with its embedding requests marked interactive.
pub async fn interactive<F: Future>(fut: F) -> F::Output {
    INTERACTIVE.scope(true, fut).await
}

/// Whether the current task is inside [`interactive`].
pub fn is_interactive() -> bool {
    INTERACTIVE.try_with(|v| *v).unwrap_or(false)
}

/// Requests one embedder may hold. Background requests beyond it are
/// refused at once (callers fall back to lexical matching); an interactive
/// one displaces the oldest background request instead.
pub const QUEUE_DEPTH: usize = 128;

struct Lanes<T> {
    interactive: VecDeque<T>,
    background: VecDeque<T>,
    closed: bool,
}

/// A bounded two-lane queue between async callers and one blocking actor
/// thread.
pub struct PriorityQueue<T> {
    lanes: Mutex<Lanes<T>>,
    ready: Condvar,
    depth: usize,
}

/// Why a request was not queued.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Full,
    Closed,
}

impl<T> PriorityQueue<T> {
    pub fn new(depth: usize) -> Self {
        Self {
            lanes: Mutex::new(Lanes {
                interactive: VecDeque::new(),
                background: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            depth,
        }
    }

    /// Queue `item`. Returns the background item it displaced, if any, so the
    /// caller can drop it outside the lock (dropping it answers its waiter).
    pub fn push(&self, item: T, interactive: bool) -> Result<Option<T>, Refused> {
        let Ok(mut lanes) = self.lanes.lock() else {
            return Err(Refused::Closed);
        };
        if lanes.closed {
            return Err(Refused::Closed);
        }
        let mut displaced = None;
        if lanes.interactive.len() + lanes.background.len() >= self.depth {
            if !interactive {
                return Err(Refused::Full);
            }
            displaced = lanes.background.pop_front();
            if displaced.is_none() {
                return Err(Refused::Full); // full of interactive work already
            }
        }
        if interactive {
            lanes.interactive.push_back(item);
        } else {
            lanes.background.push_back(item);
        }
        drop(lanes);
        self.ready.notify_one();
        Ok(displaced)
    }

    /// Block for the next item, interactive first. `None` once closed and
    /// drained.
    pub fn pop(&self) -> Option<T> {
        let mut lanes = self.lanes.lock().ok()?;
        loop {
            if let Some(item) = lanes.interactive.pop_front() {
                return Some(item);
            }
            if let Some(item) = lanes.background.pop_front() {
                return Some(item);
            }
            if lanes.closed {
                return None;
            }
            lanes = self.ready.wait(lanes).ok()?;
        }
    }

    /// Stop accepting work; the actor drains what is queued and exits.
    pub fn close(&self) {
        if let Ok(mut lanes) = self.lanes.lock() {
            lanes.closed = true;
        }
        self.ready.notify_all();
    }

    /// `(interactive, background)` waiting, for the growth counters.
    pub fn depth(&self) -> (usize, usize) {
        self.lanes
            .lock()
            .map(|l| (l.interactive.len(), l.background.len()))
            .unwrap_or((0, 0))
    }
}

/// Anything whose backlog the growth counters should report.
pub trait QueueDepth: Send + Sync {
    fn depth(&self) -> (usize, usize);
}

impl<T: Send> QueueDepth for PriorityQueue<T> {
    fn depth(&self) -> (usize, usize) {
        PriorityQueue::depth(self)
    }
}

static REGISTRY: Mutex<Vec<std::sync::Weak<dyn QueueDepth>>> = Mutex::new(Vec::new());

/// Make a queue visible to [`total_depth`]. Held weakly: a dropped embedder
/// simply stops being counted.
pub fn register(queue: &std::sync::Arc<dyn QueueDepth>) {
    if let Ok(mut r) = REGISTRY.lock() {
        r.retain(|w| w.strong_count() > 0);
        r.push(std::sync::Arc::downgrade(queue));
    }
}

/// `(interactive, background)` waiting across every live embedder.
pub fn total_depth() -> (usize, usize) {
    let Ok(r) = REGISTRY.lock() else {
        return (0, 0);
    };
    r.iter()
        .filter_map(std::sync::Weak::upgrade)
        .map(|q| q.depth())
        .fold((0, 0), |(a, b), (i, g)| (a + i, b + g))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_work_is_served_first() {
        let q = PriorityQueue::new(8);
        q.push("ingest-1", false).unwrap();
        q.push("ingest-2", false).unwrap();
        q.push("recall", true).unwrap();
        assert_eq!(q.pop(), Some("recall"));
        assert_eq!(q.pop(), Some("ingest-1"));
        assert_eq!(q.pop(), Some("ingest-2"));
    }

    #[test]
    fn a_full_queue_refuses_background_but_makes_room_for_interactive() {
        let q = PriorityQueue::new(2);
        q.push("ingest-1", false).unwrap();
        q.push("ingest-2", false).unwrap();
        assert_eq!(q.push("ingest-3", false), Err(Refused::Full));
        assert_eq!(q.push("recall", true), Ok(Some("ingest-1")), "oldest background yields");
        assert_eq!(q.depth(), (1, 1));
    }

    #[test]
    fn a_queue_full_of_interactive_work_refuses_more() {
        let q = PriorityQueue::new(1);
        q.push("recall-1", true).unwrap();
        assert_eq!(q.push("recall-2", true), Err(Refused::Full));
    }

    #[test]
    fn closing_drains_then_ends() {
        let q = PriorityQueue::new(4);
        q.push(1, false).unwrap();
        q.close();
        assert_eq!(q.push(2, false), Err(Refused::Closed));
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), None);
    }

    #[tokio::test]
    async fn the_marker_follows_the_task() {
        assert!(!is_interactive());
        assert!(interactive(async { is_interactive() }).await);
        assert!(!is_interactive());
    }
}
