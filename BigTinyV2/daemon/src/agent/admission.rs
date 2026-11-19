//! Per-app limits on how many turns run at once.
//!
//! The engine is shared, and a session runs one turn at a time, but nothing
//! bounded how many sessions one app could drive at once. Each running turn
//! holds its context, tool results and stream buffers in memory, so one app
//! opening a hundred sessions could crowd out every other app. One
//! [`AppAdmission`] per kind of work gives each app its own fixed allowance.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Held for as long as an admitted turn runs. `None` inside when the limit
/// is off.
pub type Admitted = Option<OwnedSemaphorePermit>;

/// The app already runs as many as its limit allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtLimit;

/// The two allowances the routes enforce.
pub struct Admission {
    /// Interactive `/send` turns: refused with 429 past the limit.
    pub turns: AppAdmission,
    /// Detached jobs: queued past the limit.
    pub jobs: AppAdmission,
}

impl Admission {
    pub fn from_config(agent: &crate::config::AgentConfig) -> Arc<Self> {
        Arc::new(Self {
            turns: AppAdmission::new(agent.max_turns_per_app),
            jobs: AppAdmission::new(agent.max_jobs_per_app),
        })
    }
}

pub struct AppAdmission {
    /// `0` turns the limit off.
    limit: u32,
    lanes: DashMap<String, Arc<Semaphore>>,
}

impl AppAdmission {
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            lanes: DashMap::new(),
        }
    }

    fn lane(&self, app_id: &str) -> Option<Arc<Semaphore>> {
        if self.limit == 0 {
            return None;
        }
        Some(
            self.lanes
                .entry(app_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(self.limit as usize)))
                .clone(),
        )
    }

    /// Admit now or refuse: for work someone is waiting on, who is better
    /// told "busy" than left hanging.
    pub fn try_admit(&self, app_id: &str) -> Result<Admitted, AtLimit> {
        match self.lane(app_id) {
            None => Ok(None),
            Some(sem) => sem.try_acquire_owned().map(Some).map_err(|_| AtLimit),
        }
    }

    /// Wait for room: for queued work, which is meant to be worked through
    /// rather than refused.
    pub async fn admit(&self, app_id: &str) -> Admitted {
        let sem = self.lane(app_id)?;
        // The semaphore is never closed, so this only fails if it were.
        sem.acquire_owned().await.ok()
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// `(app, running)` for every app with something running.
    pub fn usage(&self) -> Vec<(String, u32)> {
        let mut out: Vec<(String, u32)> = self
            .lanes
            .iter()
            .filter_map(|e| {
                let running = self.limit.saturating_sub(e.value().available_permits() as u32);
                (running > 0).then(|| (e.key().clone(), running))
            })
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn each_app_has_its_own_allowance() {
        let a = AppAdmission::new(2);
        let _first = a.try_admit("busy").unwrap();
        let _second = a.try_admit("busy").unwrap();
        assert!(a.try_admit("busy").is_err(), "the third concurrent turn is refused");
        assert!(a.try_admit("other").is_ok(), "another app is unaffected");
        // "other"'s permit was dropped at once, so only "busy" is running.
        assert_eq!(a.usage(), vec![("busy".to_string(), 2)]);
    }

    #[test]
    fn a_finished_turn_frees_its_place() {
        let a = AppAdmission::new(1);
        let held = a.try_admit("x").unwrap();
        assert!(a.try_admit("x").is_err());
        drop(held);
        assert!(a.try_admit("x").is_ok());
    }

    #[test]
    fn zero_means_unlimited() {
        let a = AppAdmission::new(0);
        let held: Vec<_> = (0..100).map(|_| a.try_admit("x").unwrap()).collect();
        assert!(held.iter().all(Option::is_none));
        assert!(a.usage().is_empty());
    }

    #[tokio::test]
    async fn queued_work_waits_instead_of_failing() {
        let a = Arc::new(AppAdmission::new(1));
        let held = a.admit("x").await;
        let a2 = a.clone();
        let waiter = tokio::spawn(async move { a2.admit("x").await.is_some() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "must wait while the app is at its limit");
        drop(held);
        assert!(tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap());
    }
}
