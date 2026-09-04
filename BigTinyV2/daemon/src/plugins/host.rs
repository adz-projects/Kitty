//! Per-app plugin instances, opened lazily and shut down when idle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use adaptive_pathway::embed::SemanticEmbedder;
use adaptive_pathway::engine::PathwayEngine;
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::storage::app_plugins::{self, PATHWAY};

/// One app's live pathway instance.
struct Instance {
    engine: Arc<PathwayEngine>,
    /// Stops this instance's background sweep. Dropped when the instance is
    /// shut down, which is what keeps loop count proportional to *active* apps
    /// rather than registered ones.
    shutdown: tokio::sync::watch::Sender<bool>,
    background: tokio::task::AbortHandle,
}

/// Hosts loop-integrated plugins, one instance per app.
pub struct PluginHost {
    pool: SqlitePool,
    data_dir: PathBuf,
    /// Whether pathway is on for an app that has expressed no preference.
    /// Comes from the daemon config / `BIGTINYV2_PATHWAY__ENABLED`, so an app
    /// that never opts in behaves exactly as V1 did.
    default_enabled: bool,
    ap_config: adaptive_pathway::config::Config,
    /// **Shared across every app**, by `Arc`. A loaded embedding model per app
    /// would multiply RAM by app count for no benefit, and would put each
    /// app's vectors in a different space for no reason. `None` when no
    /// embedder is configured, in which case adaptive-pathway falls back to
    /// its own lexical behaviour.
    embedder: Option<Arc<dyn SemanticEmbedder>>,
    /// The summarizer the background sweep needs. Daemon-wide: it is a
    /// provider client, not per-app state.
    ///
    /// Concrete rather than `Arc<dyn StructuredChat>` because
    /// `adaptive_pathway::background::run` is generic over `S: StructuredChat`
    /// and so requires `Sized`. Making `PluginHost` generic instead would push
    /// a type parameter through `AppState` and every route for no benefit --
    /// `SummarizerChain` is the only implementor the daemon has.
    chat: Arc<crate::agent::summarizer_chain::SummarizerChain>,
    /// Guarded by a mutex rather than a `DashMap` because opening an engine is
    /// an `await` that must not happen twice concurrently for the same app --
    /// two engines on one SQLite file is the failure this prevents.
    instances: Mutex<HashMap<String, Instance>>,
}

impl PluginHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        data_dir: PathBuf,
        default_enabled: bool,
        ap_config: adaptive_pathway::config::Config,
        embedder: Option<Arc<dyn SemanticEmbedder>>,
        chat: Arc<crate::agent::summarizer_chain::SummarizerChain>,
    ) -> Self {
        Self {
            pool,
            data_dir,
            default_enabled,
            ap_config,
            embedder,
            chat,
            instances: Mutex::new(HashMap::new()),
        }
    }

    /// Where an app's belief graph lives.
    ///
    /// Nested under `apps/<app_id>/` rather than `pathway-<app_id>.db` so
    /// everything else an app accumulates has an obvious home later, and so a
    /// revoked app's state can be removed with one directory delete.
    fn db_path(&self, app_id: &str) -> PathBuf {
        self.data_dir.join("apps").join(app_id).join("pathway.db")
    }

    /// Whether pathway is on for this app: its own preference, else the
    /// daemon default.
    pub async fn is_enabled(&self, app_id: &str) -> bool {
        match app_plugins::is_enabled(&self.pool, app_id, PATHWAY).await {
            Ok(Some(explicit)) => explicit,
            Ok(None) => self.default_enabled,
            Err(e) => {
                // Fall back to the default rather than failing the turn: this
                // is a memory feature, not a correctness one.
                tracing::warn!("could not read plugin preference for {app_id}: {e}");
                self.default_enabled
            }
        }
    }

    /// This app's engine, opening one if needed. `None` when pathway is off
    /// for the app, or when the engine could not be opened.
    ///
    /// Lazy on purpose: a registered app that never sends a turn should cost
    /// neither a SQLite file nor a background task.
    pub async fn pathway_for(&self, app_id: &str) -> Option<Arc<PathwayEngine>> {
        // An already-open instance short-circuits the preference check: it is
        // either genuinely in use, or was injected by a test as an explicit
        // "use this". Turning a plugin off closes its instance (`close`)
        // rather than orphaning a live engine behind a false check.
        if let Some(existing) = self.instances.lock().await.get(app_id) {
            return Some(existing.engine.clone());
        }
        if !self.is_enabled(app_id).await {
            return None;
        }

        let mut instances = self.instances.lock().await;
        if let Some(existing) = instances.get(app_id) {
            return Some(existing.engine.clone());
        }

        let db_path = self.db_path(app_id);
        if let Some(parent) = db_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!("could not create plugin dir {parent:?}: {e}");
                return None;
            }
        }

        let engine = match PathwayEngine::open_with_embedder(
            &db_path.to_string_lossy(),
            self.ap_config.clone(),
            self.embedder.clone(),
        )
        .await
        {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("pathway engine failed to open at {db_path:?}: {err}");
                return None;
            }
        };

        // One background sweep per *live* engine. V1 spawned exactly one
        // because there was exactly one engine; naively carrying that forward
        // would mean an idle-sweep loop per registered app forever.
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let background = tokio::spawn(adaptive_pathway::background::run(
            engine.clone(),
            self.pool.clone(),
            self.chat.clone(),
            rx,
        ))
        .abort_handle();

        tracing::info!(app_id, path = ?db_path, "opened pathway instance");
        instances.insert(
            app_id.to_string(),
            Instance {
                engine: engine.clone(),
                shutdown,
                background,
            },
        );
        Some(engine)
    }

    /// Close one app's instance and stop its background work.
    ///
    /// Signals the sweep to stop before aborting it, so a sweep mid-write gets
    /// the chance to finish rather than being torn out of a transaction.
    pub async fn close(&self, app_id: &str) {
        let Some(instance) = self.instances.lock().await.remove(app_id) else {
            return;
        };
        let _ = instance.shutdown.send(true);
        instance.background.abort();
        tracing::info!(app_id, "closed pathway instance");
    }

    /// Close every instance. Called on daemon shutdown.
    pub async fn shutdown(&self) {
        let mut instances = self.instances.lock().await;
        for (app_id, instance) in instances.drain() {
            let _ = instance.shutdown.send(true);
            instance.background.abort();
            tracing::debug!(app_id, "closed pathway instance on shutdown");
        }
    }

    /// How many instances are currently open. Diagnostics, and the thing the
    /// resource-shape test asserts on.
    pub async fn open_instances(&self) -> usize {
        self.instances.lock().await.len()
    }

    /// Register a prebuilt engine for `app_id`, bypassing the open path.
    ///
    /// For tests that want to seed a belief graph (typically an in-memory one)
    /// and then exercise the routes against it. No background sweep is
    /// attached, because a test does not want one running underneath it.
    ///
    /// Overrides the enabled check for that app: an injected engine is an
    /// explicit "use this", so `pathway_for` returns it regardless of
    /// preference.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn insert_for_test(&self, app_id: &str, engine: Arc<PathwayEngine>) {
        let (shutdown, _rx) = tokio::sync::watch::channel(false);
        // A task that ends immediately: `Instance` wants an abort handle, and
        // aborting an already-finished task is a no-op.
        let background = tokio::spawn(async {}).abort_handle();
        self.instances.lock().await.insert(
            app_id.to_string(),
            Instance {
                engine,
                shutdown,
                background,
            },
        );
    }

    /// The shared embedder, for `/api/embeddings`.
    ///
    /// Exposed rather than duplicated: the model is already loaded for
    /// pathway, and serving embeddings from a second copy would double the
    /// RAM for no reason.
    pub fn embedder(&self) -> Option<Arc<dyn SemanticEmbedder>> {
        self.embedder.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `SummarizerChain` over an empty provider registry: the sweep
    /// holds it but never reaches a provider, because there are no beliefs to
    /// work on in these tests.
    fn null_chat() -> Arc<crate::agent::summarizer_chain::SummarizerChain> {
        let config = crate::config::BigTinyConfig::default();
        Arc::new(crate::agent::summarizer_chain::SummarizerChain::new(
            None,
            Arc::new(crate::provider::router::ProviderRouter::new(
                config.cache.clone(),
            )),
            config.summarizer.clone(),
        ))
    }

    async fn host(dir: &std::path::Path, default_enabled: bool) -> (PluginHost, SqlitePool) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for app in ["app-a", "app-b"] {
            crate::storage::apps::register_app(&pool, app, app, &format!("key-{app}"))
                .await
                .unwrap();
        }
        let host = PluginHost::new(
            pool.clone(),
            dir.to_path_buf(),
            default_enabled,
            adaptive_pathway::config::Config::default(),
            None,
            null_chat(),
        );
        (host, pool)
    }

    #[tokio::test]
    async fn each_app_gets_its_own_database_file() {
        // The core of the split: two apps, two graphs, no shared beliefs.
        let dir = tempfile::tempdir().unwrap();
        let (host, _pool) = host(dir.path(), true).await;

        assert!(host.pathway_for("app-a").await.is_some());
        assert!(host.pathway_for("app-b").await.is_some());

        assert!(dir.path().join("apps/app-a/pathway.db").exists());
        assert!(dir.path().join("apps/app-b/pathway.db").exists());
        assert_eq!(host.open_instances().await, 2);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn an_instance_is_reused_not_reopened() {
        // Two engines on one SQLite file would be a corruption risk, and a
        // second background sweep per turn would be a leak.
        let dir = tempfile::tempdir().unwrap();
        let (host, _pool) = host(dir.path(), true).await;

        let first = host.pathway_for("app-a").await.unwrap();
        let second = host.pathway_for("app-a").await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(host.open_instances().await, 1);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn a_disabled_app_opens_nothing_at_all() {
        // Not merely "tools hidden": no engine, no file, no background loop.
        // Turning pathway off must actually cost zero.
        let dir = tempfile::tempdir().unwrap();
        let (host, pool) = host(dir.path(), true).await;
        app_plugins::set_enabled(&pool, "app-a", PATHWAY, false)
            .await
            .unwrap();

        assert!(host.pathway_for("app-a").await.is_none());
        assert_eq!(host.open_instances().await, 0);
        assert!(!dir.path().join("apps/app-a").exists());
    }

    #[tokio::test]
    async fn the_daemon_default_applies_only_without_an_explicit_preference() {
        let dir = tempfile::tempdir().unwrap();
        let (host, pool) = host(dir.path(), false).await;

        // Default off, no preference -> off.
        assert!(!host.is_enabled("app-a").await);

        // Explicit opt-in beats a default of off...
        app_plugins::set_enabled(&pool, "app-a", PATHWAY, true)
            .await
            .unwrap();
        assert!(host.is_enabled("app-a").await);

        // ...and app-b is unaffected by app-a's choice.
        assert!(!host.is_enabled("app-b").await);
    }

    #[tokio::test]
    async fn closing_an_instance_frees_it_and_it_can_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _pool) = host(dir.path(), true).await;

        host.pathway_for("app-a").await.unwrap();
        assert_eq!(host.open_instances().await, 1);

        host.close("app-a").await;
        assert_eq!(host.open_instances().await, 0);

        // Reopening must work — an idle-closed app that becomes active again
        // is the normal case, not an error.
        assert!(host.pathway_for("app-a").await.is_some());
        assert_eq!(host.open_instances().await, 1);
        host.shutdown().await;
    }

    #[tokio::test]
    async fn closing_an_unopened_app_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (host, _pool) = host(dir.path(), true).await;
        host.close("never-opened").await;
        assert_eq!(host.open_instances().await, 0);
    }
}
