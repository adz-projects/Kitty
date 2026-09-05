//! Per-turn event buffers, so a client can rejoin a stream it dropped.
//!
//! # Why this exists now
//!
//! In V1 a turn's events went to exactly one consumer and were gone once sent.
//! That was defensible while every turn had a live client holding the stream:
//! lose the connection and the turn was cancelled anyway.
//!
//! Detached jobs break that assumption — work now outlives its submitter — so a
//! client needs a way back to a turn already in progress. This is the buffer
//! that makes `Last-Event-ID` and `GET /api/chat/{id}/stream` possible.
//!
//! **This reverses an earlier decision.** Fan-out was originally out of scope
//! on the grounds that each app owns its own sessions and a second `/send`
//! correctly 409s. That reasoning was about two clients *starting* work; it
//! says nothing about one client rejoining work already running, which is
//! exactly what a detached job requires.
//!
//! # What is and is not replayable
//!
//! Structural events -- tool calls, errors, status, terminal frames -- are
//! buffered unconditionally: they are what a resuming client needs to
//! reconstruct state, and they are small and rare.
//!
//! Text deltas are best-effort. The SSE send path already drops non-final
//! `LlmDelta`/`ReasoningDelta` under queue pressure rather than stalling the
//! turn, so a dropped delta was never recoverable in the first place; buffering
//! them unboundedly would only move the memory problem. A resuming client is
//! told when text was lost rather than handed a silently incomplete
//! transcript -- a gap it knows about can be repaired by reading the
//! transcript, one it does not know about cannot.

use std::collections::VecDeque;
use std::sync::Arc;

use dashmap::DashMap;

use super::events::{SSEEvent, SSEEventType};

/// Events retained per turn.
///
/// Sized for a rejoin after a brief drop, not for replaying a whole turn from
/// nothing: a resuming client that has fallen further behind than this reads
/// the transcript instead, which is the durable record either way.
const BUFFER_CAPACITY: usize = 512;

/// One event, with the id a client resumes from.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub id: u64,
    pub event: SSEEvent,
}

struct TurnBuffer {
    events: VecDeque<Recorded>,
    next_id: u64,
    /// Text deltas dropped rather than buffered, so a resuming client can be
    /// told its transcript has a hole.
    dropped_deltas: u64,
    /// Set once a terminal frame is recorded. A resuming client can then be
    /// served the tail and closed immediately rather than waiting on a turn
    /// that has already finished.
    finished: bool,
}

impl TurnBuffer {
    fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(64),
            next_id: 1,
            dropped_deltas: 0,
            finished: false,
        }
    }
}

/// Whether an event must survive for a resuming client to make sense of the
/// turn.
///
/// Deltas are the only best-effort class; everything else changes what the
/// client believes about the turn's state.
fn is_structural(event: &SSEEvent) -> bool {
    !matches!(
        event.event_type,
        SSEEventType::LlmDelta | SSEEventType::ReasoningDelta
    ) || event.is_last
}

/// Live turn buffers, keyed by session.
#[derive(Default)]
pub struct ReplayBuffers {
    turns: DashMap<String, TurnBuffer>,
}

impl ReplayBuffers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start (or restart) a session's buffer. Called when a turn begins.
    pub fn begin(&self, session_id: &str) {
        self.turns.insert(session_id.to_string(), TurnBuffer::new());
    }

    /// Record an event and return the id assigned to it.
    ///
    /// Returns `None` when the event was not buffered, which is not a failure:
    /// text deltas are deliberately best-effort.
    pub fn record(&self, session_id: &str, event: &SSEEvent) -> Option<u64> {
        let mut buf = self.turns.get_mut(session_id)?;
        if !is_structural(event) {
            // Counted, not stored. See the module docs.
            buf.dropped_deltas += 1;
            return None;
        }
        let id = buf.next_id;
        buf.next_id += 1;
        if event.is_last {
            buf.finished = true;
        }
        buf.events.push_back(Recorded {
            id,
            event: event.clone(),
        });
        while buf.events.len() > BUFFER_CAPACITY {
            buf.events.pop_front();
        }
        Some(id)
    }

    /// Events after `last_id`, plus whether any text was lost.
    ///
    /// A `last_id` older than the buffer's oldest retained event yields
    /// `truncated = true`: the client is behind by more than the buffer holds,
    /// and must reconcile from the transcript rather than assume continuity.
    pub fn since(&self, session_id: &str, last_id: Option<u64>) -> Option<Replay> {
        let buf = self.turns.get(session_id)?;
        let oldest = buf.events.front().map(|r| r.id);

        let truncated = match (last_id, oldest) {
            // `last_id` comes straight off a client's `Last-Event-ID` header,
            // so `u64::MAX` is reachable input, and `last + 1` panics on it in
            // a debug build. Saturating there means "you are not behind",
            // which is the right reading: an id above everything retained
            // cannot be older than the oldest event held.
            (Some(last), Some(oldest)) => last.checked_add(1).is_some_and(|n| n < oldest),
            _ => false,
        };
        let events: Vec<Recorded> = match last_id {
            Some(last) => buf.events.iter().filter(|r| r.id > last).cloned().collect(),
            None => buf.events.iter().cloned().collect(),
        };
        Some(Replay {
            events,
            truncated,
            dropped_deltas: buf.dropped_deltas,
            finished: buf.finished,
        })
    }

    /// Whether a turn is buffered and still running.
    pub fn is_live(&self, session_id: &str) -> bool {
        self.turns
            .get(session_id)
            .map(|b| !b.finished)
            .unwrap_or(false)
    }

    /// Drop a session's buffer.
    ///
    /// Not called on turn end: a client that reconnects a moment after the
    /// turn finished still wants the tail. Buffers are replaced by the next
    /// `begin`, and dropped when the session is.
    pub fn forget(&self, session_id: &str) {
        self.turns.remove(session_id);
    }
}

/// What a resuming client is given.
#[derive(Debug)]
pub struct Replay {
    pub events: Vec<Recorded>,
    /// The client is further behind than the buffer retains.
    pub truncated: bool,
    /// Text deltas dropped during the turn.
    pub dropped_deltas: u64,
    /// The turn has already produced its terminal frame.
    pub finished: bool,
}

impl Replay {
    /// Whether the client should be told its transcript has a gap.
    pub fn has_gap(&self) -> bool {
        self.truncated || self.dropped_deltas > 0
    }
}

/// Shared handle.
pub type SharedReplay = Arc<ReplayBuffers>;

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> SSEEvent {
        SSEEvent::content(text)
    }

    fn tool() -> SSEEvent {
        SSEEvent {
            event_type: SSEEventType::ToolStart,
            tool_name: Some("read_file".into()),
            ..Default::default()
        }
    }

    fn terminal() -> SSEEvent {
        SSEEvent {
            event_type: SSEEventType::SessionStatus,
            content: Some("Completed".into()),
            is_last: true,
            ..Default::default()
        }
    }

    #[test]
    fn structural_events_are_buffered_and_deltas_are_not() {
        let b = ReplayBuffers::new();
        b.begin("s1");

        assert_eq!(b.record("s1", &tool()), Some(1));
        assert_eq!(b.record("s1", &delta("hello")), None);
        assert_eq!(b.record("s1", &tool()), Some(2));

        let replay = b.since("s1", None).unwrap();
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.dropped_deltas, 1);
        assert!(replay.has_gap(), "a dropped delta is a gap worth reporting");
    }

    #[test]
    fn a_final_delta_is_kept_because_it_is_terminal() {
        // `is_last` marks the frame a client needs to know the turn ended;
        // dropping it would leave a resuming client waiting forever.
        let b = ReplayBuffers::new();
        b.begin("s1");
        let mut last = delta("tail");
        last.is_last = true;
        assert_eq!(b.record("s1", &last), Some(1));
        assert!(b.since("s1", None).unwrap().finished);
    }

    #[test]
    fn resuming_returns_only_events_after_the_given_id() {
        let b = ReplayBuffers::new();
        b.begin("s1");
        for _ in 0..5 {
            b.record("s1", &tool());
        }
        let replay = b.since("s1", Some(3)).unwrap();
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.events[0].id, 4);
        assert!(!replay.truncated);
    }

    #[test]
    fn falling_further_behind_than_the_buffer_is_reported_as_truncated() {
        // The client must reconcile from the transcript rather than assume the
        // events it never saw did not happen.
        let b = ReplayBuffers::new();
        b.begin("s1");
        for _ in 0..(BUFFER_CAPACITY + 50) {
            b.record("s1", &tool());
        }
        let replay = b.since("s1", Some(1)).unwrap();
        assert!(replay.truncated);
        assert!(replay.has_gap());
    }

    #[test]
    fn the_buffer_is_bounded() {
        let b = ReplayBuffers::new();
        b.begin("s1");
        for _ in 0..(BUFFER_CAPACITY * 3) {
            b.record("s1", &tool());
        }
        assert_eq!(b.since("s1", None).unwrap().events.len(), BUFFER_CAPACITY);
    }

    #[test]
    fn a_finished_turn_is_still_replayable() {
        // A client reconnecting just after the turn ended wants the tail, so
        // buffers are not dropped on completion.
        let b = ReplayBuffers::new();
        b.begin("s1");
        b.record("s1", &tool());
        b.record("s1", &terminal());

        assert!(!b.is_live("s1"));
        let replay = b.since("s1", None).unwrap();
        assert!(replay.finished);
        assert_eq!(replay.events.len(), 2);
    }

    #[test]
    fn a_new_turn_resets_the_buffer() {
        // Ids restart per turn, so a stale `Last-Event-ID` from a previous turn
        // cannot skip past the new one's events.
        let b = ReplayBuffers::new();
        b.begin("s1");
        for _ in 0..5 {
            b.record("s1", &tool());
        }
        b.begin("s1");
        assert_eq!(b.record("s1", &tool()), Some(1));
        assert_eq!(b.since("s1", None).unwrap().events.len(), 1);
    }

    #[test]
    fn an_unknown_session_has_nothing_to_replay() {
        let b = ReplayBuffers::new();
        assert!(b.since("never-seen", None).is_none());
        assert!(!b.is_live("never-seen"));
        assert_eq!(b.record("never-seen", &tool()), None);
    }

    #[test]
    fn a_max_value_last_event_id_neither_panics_nor_reports_truncation() {
        // `Last-Event-ID` is a client-supplied header, so `u64::MAX` is
        // reachable input rather than a hypothetical. `last + 1` panicked on
        // it in debug builds -- a remote client could stop a turn's resume
        // path with one header.
        let b = ReplayBuffers::new();
        b.begin("s1");
        b.record("s1", &tool());

        let out = b.since("s1", Some(u64::MAX)).expect("buffer exists");
        assert!(!out.truncated, "an id above everything held is not behind");
        assert!(out.events.is_empty(), "nothing is newer than u64::MAX");
    }
}
