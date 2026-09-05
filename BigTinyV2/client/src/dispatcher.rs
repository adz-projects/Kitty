//! Submission pacing for pipeline-style callers.
//!
//! Hand it two hundred jobs and it keeps each provider busy without burying
//! the daemon, reading the endpoint's real slot count rather than guessing.
//!
//! # Why pace at all, when the daemon already queues
//!
//! The daemon's `ProviderQueue` is the authority on fairness and is not
//! duplicated here. What it cannot do is stop a client opening two hundred
//! simultaneous HTTP requests that then sit waiting: that costs sockets, file
//! descriptors and memory on both sides for no benefit.
//!
//! So this bounds *in-flight submissions*, not scheduling. Ordering, fairness
//! and priority stay server-side, where they can see every app rather than
//! just this one.
//!
//! # Across endpoints
//!
//! Providers are paced independently and dispatched concurrently: work pinned
//! to a saturated provider must never hold up work destined for an idle one.
//! That is the difference between "several endpoints" and "several endpoints
//! in parallel".

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::client::BigTinyClient;
use crate::error::Result;

/// Fallback slot count for a provider whose concurrency could not be read.
///
/// One, not more: guessing high against a single-slot endpoint is how a client
/// buries a daemon, and the cost of guessing low is only a little idle
/// capacity that the next refresh corrects.
const UNKNOWN_CONCURRENCY: u32 = 1;

/// One unit of work.
#[derive(Debug, Clone)]
pub struct Job {
    pub prompt: String,
    /// Pin to a provider. `None` lets the daemon resolve it, in which case
    /// this job is paced against the app's default provider.
    pub provider_id: Option<String>,
    pub parent_session_id: Option<String>,
}

impl Job {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            provider_id: None,
            parent_session_id: None,
        }
    }

    pub fn on_provider(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
    }

    pub fn under(mut self, parent_session_id: impl Into<String>) -> Self {
        self.parent_session_id = Some(parent_session_id.into());
        self
    }
}

/// What a job produced.
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub job_id: String,
    pub session_id: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl JobOutcome {
    pub fn succeeded(&self) -> bool {
        self.status == "succeeded"
    }
}

/// Paces submissions per provider.
pub struct Dispatcher {
    client: BigTinyClient,
    /// One semaphore per provider, sized to that endpoint's slot count. Keyed
    /// separately from the client so a refresh can replace the *count* without
    /// disturbing work already in flight.
    gates: HashMap<String, Arc<Semaphore>>,
    /// Used for jobs that name no provider.
    default_gate: Arc<Semaphore>,
}

impl Dispatcher {
    /// Build a dispatcher, reading each provider's current slot count.
    ///
    /// A provider whose concurrency cannot be read falls back to
    /// [`UNKNOWN_CONCURRENCY`] rather than failing: a pipeline should degrade
    /// to slower, not to broken.
    pub async fn new(client: BigTinyClient) -> Result<Self> {
        let providers = client.providers().await.unwrap_or_default();
        let mut gates = HashMap::new();
        let mut total = 0u32;

        for p in &providers {
            let slots = p.concurrency.max(1);
            total += slots;
            gates.insert(p.id.clone(), Arc::new(Semaphore::new(slots as usize)));
        }

        // Jobs with no pinned provider share one gate sized to the whole
        // visible capacity: the daemon decides which endpoint each lands on,
        // so pacing them per-endpoint here would be guessing.
        let default_slots = total.max(UNKNOWN_CONCURRENCY) as usize;
        Ok(Self {
            client,
            gates,
            default_gate: Arc::new(Semaphore::new(default_slots)),
        })
    }

    fn gate_for(&self, provider_id: Option<&str>) -> Arc<Semaphore> {
        provider_id
            .and_then(|id| self.gates.get(id))
            .cloned()
            .unwrap_or_else(|| self.default_gate.clone())
    }

    /// Total slots across every visible provider.
    pub fn capacity(&self) -> usize {
        self.default_gate.available_permits()
            + self
                .gates
                .values()
                .map(|g| g.available_permits())
                .sum::<usize>()
    }

    /// Run every job, respecting each provider's slot count, and return the
    /// outcomes in submission order.
    ///
    /// Order is preserved because a caller almost always needs to line results
    /// up against the inputs that produced them; completion order would make
    /// that the caller's problem for no gain.
    pub async fn run_all(
        &self,
        jobs: Vec<Job>,
        per_job_timeout: std::time::Duration,
    ) -> Vec<Result<JobOutcome>> {
        let futures = jobs.into_iter().map(|job| {
            let gate = self.gate_for(job.provider_id.as_deref());
            let client = self.client.clone();
            async move {
                // Held across submit *and* await: releasing at submit would
                // let the client pile up unbounded in-flight work, which is
                // exactly what the gate exists to prevent.
                let _permit = gate
                    .acquire()
                    .await
                    .map_err(|_| crate::ClientError::NotFound("dispatcher gate closed".into()))?;

                let (job_id, session_id) = client
                    .submit_job(&job.prompt, job.parent_session_id.as_deref())
                    .await?;
                let finished = client.await_job(&job_id, per_job_timeout).await?;

                Ok(JobOutcome {
                    job_id,
                    session_id,
                    status: finished["status"].as_str().unwrap_or_default().to_string(),
                    result: finished["result"].as_str().map(str::to_owned),
                    error: finished["error"].as_str().map(str::to_owned),
                })
            }
        });

        // `join_all` preserves input order regardless of completion order, and
        // different providers' futures progress independently -- which is what
        // keeps a saturated endpoint from holding up an idle one.
        futures::future::join_all(futures).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_builds_up_fluently() {
        let job = Job::new("do it").on_provider("p1").under("parent");
        assert_eq!(job.prompt, "do it");
        assert_eq!(job.provider_id.as_deref(), Some("p1"));
        assert_eq!(job.parent_session_id.as_deref(), Some("parent"));
    }

    #[test]
    fn an_outcome_reports_success_only_for_the_terminal_success_state() {
        let outcome = |status: &str| JobOutcome {
            job_id: "j".into(),
            session_id: "s".into(),
            status: status.into(),
            result: None,
            error: None,
        };
        assert!(outcome("succeeded").succeeded());
        for bad in ["failed", "cancelled", "interrupted", "running"] {
            assert!(!outcome(bad).succeeded(), "{bad} is not success");
        }
    }

    #[tokio::test]
    async fn a_pinned_provider_uses_its_own_gate_and_an_unpinned_job_the_default() {
        let client = BigTinyClient::new("http://127.0.0.1:1", "k");
        let mut gates = HashMap::new();
        gates.insert("p1".to_string(), Arc::new(Semaphore::new(4)));
        let d = Dispatcher {
            client,
            gates,
            default_gate: Arc::new(Semaphore::new(2)),
        };

        assert_eq!(d.gate_for(Some("p1")).available_permits(), 4);
        // An unknown provider falls back to the default gate rather than
        // creating an unbounded one.
        assert_eq!(d.gate_for(Some("never-seen")).available_permits(), 2);
        assert_eq!(d.gate_for(None).available_permits(), 2);
    }

    #[tokio::test]
    async fn a_dispatcher_against_an_unreachable_daemon_still_builds() {
        // A pipeline should degrade to slower, not to broken: `new` reads
        // provider concurrency as a hint, and a failure to read it must not
        // stop work being submitted.
        let client = BigTinyClient::new("http://127.0.0.1:1", "k");
        let d = Dispatcher::new(client).await.expect("should not fail");
        assert!(d.capacity() >= UNKNOWN_CONCURRENCY as usize);
    }
}
