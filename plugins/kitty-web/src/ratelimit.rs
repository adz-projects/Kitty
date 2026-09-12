//! Process-wide pacing for the scraped search engines.
//!
//! Both DuckDuckGo and Bing answer an unauthenticated scrape, which means the
//! only thing standing between this crate and a bot challenge is how fast it
//! asks. That rate is not something any single call site can see: BigTiny
//! holds **one** `kitty-web` client for the whole daemon
//! (`MCPServerManager::servers` is a `DashMap<String, Arc<MCPServerClient>>`),
//! so the main agent and every `call_specialist` delegate funnel their tool
//! calls through this one process. Three parallel `researcher` specialists
//! therefore burst three concurrent searches at an engine that is watching for
//! exactly that, and each of them then *reworded and retried* when the
//! challenge came back looking like "no results" — a self-reinforcing spiral
//! that emptied a 300s specialist budget without returning anything.
//!
//! A process-global limiter is the right altitude precisely because the
//! process boundary is where all those callers meet.
//!
//! Per-engine, not one shared bucket: DuckDuckGo's pacing must never make Bing
//! wait, since covering DuckDuckGo's challenged fraction is Bing's entire job.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Minimum spacing between two DuckDuckGo requests.
const DDG_MIN_INTERVAL: Duration = Duration::from_millis(800);
/// Minimum spacing between two Bing requests.
const BING_MIN_INTERVAL: Duration = Duration::from_millis(600);

/// How many requests may go out back-to-back after an idle period. A single
/// interactive search should never feel paced; only sustained fan-out should.
/// Must be at least 1.
const BURST: u32 = 2;

/// Ceiling on how long `acquire` will hold a caller in the queue.
///
/// Same reasoning as `search::MAX_RETRY_AFTER_SECONDS`: the model's turn is
/// blocked behind this tool call, so an unbounded queue converts a rate limit
/// into a hang. Past the ceiling the caller is told the engine is unavailable,
/// which is both true and immediately actionable — the *other* engine's
/// results still come back.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(20);

/// Returned when a caller would have had to wait longer than `MAX_QUEUE_WAIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueTimeout;

/// One engine's pacing state.
///
/// `next_free` is the instant at which the next request may go out. Reserving
/// a slot and sleeping until it are deliberately separate steps: the lock is
/// released before the sleep, so N concurrent callers each take a distinct
/// slot in one pass instead of serializing their *waits* behind one another.
pub struct EngineLimiter {
    min_interval: Duration,
    burst_allowance: Duration,
    next_free: Mutex<Instant>,
}

impl EngineLimiter {
    fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            // `BURST - 1`, not `BURST`: one request is always due immediately
            // (the slot at `now`), so the banked credit only has to cover the
            // *additional* back-to-back ones.
            burst_allowance: min_interval * (BURST - 1),
            next_free: Mutex::new(Instant::now()),
        }
    }

    /// Waits until this engine's next slot is due.
    ///
    /// Returns `Err(QueueTimeout)` without consuming a slot when the queue is
    /// already longer than `MAX_QUEUE_WAIT`.
    pub async fn acquire(&self) -> Result<(), QueueTimeout> {
        let now = Instant::now();
        // Reserving the slot and sleeping until it are separate steps, and the
        // lock is held only for the first: N concurrent callers each take a
        // distinct slot in one pass instead of serializing their *waits*.
        let slot = {
            let mut next_free = self.next_free.lock().await;
            reserve_slot(now, &mut next_free, self.min_interval, self.burst_allowance)?
        };

        tokio::time::sleep_until(slot).await;
        Ok(())
    }
}

/// Claims the next slot, advancing `next_free` past it.
///
/// Pure and synchronous on purpose. Under `#[tokio::test(start_paused)]` a
/// sleep auto-advances the virtual clock, so an `acquire` loop can never build
/// up a queue to test the ceiling against — the clock chases each slot. Taking
/// the reservation rule out of the async wrapper lets it be asserted directly.
fn reserve_slot(
    now: Instant,
    next_free: &mut Instant,
    min_interval: Duration,
    burst_allowance: Duration,
) -> Result<Instant, QueueTimeout> {
    // Credit accrued while idle, capped at `burst_allowance` so an engine left
    // alone overnight doesn't bank unlimited burst.
    let earliest = now.checked_sub(burst_allowance).unwrap_or(now);
    if *next_free < earliest {
        *next_free = earliest;
    }

    let slot = *next_free;
    if slot.saturating_duration_since(now) > MAX_QUEUE_WAIT {
        // Leave `next_free` untouched — a caller that gives up must not push
        // the queue out for everyone behind it.
        return Err(QueueTimeout);
    }
    *next_free = slot + min_interval;
    Ok(slot)
}

fn limiters() -> &'static HashMap<&'static str, Arc<EngineLimiter>> {
    static LIMITERS: OnceLock<HashMap<&'static str, Arc<EngineLimiter>>> = OnceLock::new();
    LIMITERS.get_or_init(|| {
        HashMap::from([
            ("duckduckgo", Arc::new(EngineLimiter::new(DDG_MIN_INTERVAL))),
            ("bing", Arc::new(EngineLimiter::new(BING_MIN_INTERVAL))),
        ])
    })
}

/// Waits for `engine`'s next slot. An engine with no configured limiter (none
/// today; Brave paces itself through its own API quota) proceeds immediately.
pub async fn acquire(engine: &str) -> Result<(), QueueTimeout> {
    match limiters().get(engine) {
        Some(limiter) => limiter.acquire().await,
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_are_spaced_by_the_min_interval() {
        let limiter = Arc::new(EngineLimiter::new(Duration::from_millis(800)));
        let start = Instant::now();

        // More callers than the burst allowance, all arriving at once — the
        // parallel-specialist shape this exists for.
        let mut handles = Vec::new();
        for _ in 0..5 {
            let l = limiter.clone();
            handles.push(tokio::spawn(async move {
                l.acquire().await.expect("within the queue ceiling");
                Instant::now()
            }));
        }

        let mut times = Vec::new();
        for h in handles {
            times.push(h.await.unwrap());
        }
        times.sort();

        // BURST (2) go immediately; the rest are spaced out behind them.
        let last = times.last().unwrap().duration_since(start);
        assert!(
            last >= Duration::from_millis(800 * 2),
            "5 concurrent calls finished too fast to have been paced: {last:?}"
        );

        for pair in times.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            assert!(
                gap == Duration::ZERO || gap >= Duration::from_millis(800),
                "adjacent calls {gap:?} apart — neither bursted nor paced"
            );
        }
    }

    #[test]
    fn a_full_queue_is_refused_rather_than_parked() {
        let interval = Duration::from_secs(5);
        let now = Instant::now();
        let mut next_free = now;
        let burst = interval * (BURST - 1);

        // Reserve until the queue runs past the 20s ceiling.
        let mut granted = 0;
        while reserve_slot(now, &mut next_free, interval, burst).is_ok() {
            granted += 1;
            assert!(granted < 20, "the queue ceiling never refused anyone");
        }

        // One immediate slot, then one per interval up to the 20s ceiling
        // (20 / 5 = 4). No idle credit applies: `next_free` starts at `now`,
        // so this limiter has not been sitting unused.
        assert_eq!(granted, 5);
    }

    #[test]
    fn idle_time_banks_burst_credit_but_not_without_limit() {
        let interval = Duration::from_secs(5);
        let burst = interval * (BURST - 1);
        let now = Instant::now();

        // An hour idle must not bank an hour of burst.
        let mut next_free = now - Duration::from_secs(3600);
        for _ in 0..BURST {
            let slot = reserve_slot(now, &mut next_free, interval, burst)
                .expect("banked credit should be granted immediately");
            assert!(slot <= now, "banked slot should be due already");
        }
        let next = reserve_slot(now, &mut next_free, interval, burst).expect("within ceiling");
        assert!(
            next > now,
            "credit beyond the burst allowance was banked: {:?}",
            next.duration_since(now)
        );
    }

    #[test]
    fn a_refused_caller_does_not_extend_the_queue() {
        let interval = Duration::from_secs(5);
        let now = Instant::now();
        let mut next_free = now;

        while reserve_slot(now, &mut next_free, interval, interval * (BURST - 1)).is_ok() {}

        let after_refusal = next_free;
        assert_eq!(
            reserve_slot(now, &mut next_free, interval, interval * (BURST - 1)),
            Err(QueueTimeout)
        );
        assert_eq!(
            next_free, after_refusal,
            "a refused caller pushed the queue out for everyone behind it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_engine_is_not_paced() {
        let start = Instant::now();
        acquire("brave").await.expect("no limiter configured");
        assert_eq!(Instant::now(), start);
    }
}
