//! Request middleware: identity resolution and logging.
//!
//! # What changed from V1, and why
//!
//! V1's auth was a single shared secret compared against `X-API-Key`, with a
//! boolean outcome: allowed or 401. That is exactly right for one client and
//! useless for several — every holder of the key had full access to every
//! session, every provider row, and MCP server registration (i.e. arbitrary
//! process spawn via a `stdio` server), with no way to tell two callers apart.
//!
//! V2 resolves the key to an [`AppIdentity`] instead and puts it in the request
//! extensions, so every handler knows *who* is asking and can scope its query.
//! The 401 path is unchanged; what is new is that success now carries a name.
//!
//! **There is no legacy single-secret compatibility mode.** V2 is a fork with
//! no existing clients, so the "matches the old `BIGTINY_SECRET` → assume it's
//! Kitty" path that an in-place change would have needed simply does not exist
//! here. Kitty adopts registration when it migrates.
//!
//! # The key cache
//!
//! Auth runs on every request, and a SQLite round-trip per request would be a
//! silly cost for a value that changes only at registration. Resolved
//! identities are cached by key hash. Revocation invalidates the cache
//! explicitly (see [`KeyCache::invalidate`]) rather than relying on a TTL,
//! because "deleted app can still make requests for the next N seconds" is not
//! an acceptable window for a credential.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use dashmap::DashMap;
use serde_json::json;
use sqlx::SqlitePool;

use crate::storage::apps::{self, AppIdentity};

/// How often an app's `last_seen_at` is written.
///
/// Coarse on purpose: this is liveness telemetry feeding the idle-exit timer,
/// not an audit log, and writing it per request would add a database write to
/// every single call — including every SSE delta's parent request.
const LAST_SEEN_INTERVAL: Duration = Duration::from_secs(60);

/// Resolved-identity cache, keyed by the presented key's hash.
#[derive(Default)]
pub struct KeyCache {
    entries: DashMap<String, (AppIdentity, Instant)>,
}

impl KeyCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, key_hash: &str) -> Option<AppIdentity> {
        self.entries.get(key_hash).map(|e| e.0.clone())
    }

    fn insert(&self, key_hash: String, identity: AppIdentity) {
        self.entries.insert(key_hash, (identity, Instant::now()));
    }

    /// Whether enough time has passed to bother writing `last_seen_at`.
    /// Bumps the timestamp as a side effect, so callers race at worst into a
    /// duplicate write rather than into a lost one.
    fn should_touch(&self, key_hash: &str) -> bool {
        let Some(mut entry) = self.entries.get_mut(key_hash) else {
            return false;
        };
        if entry.1.elapsed() >= LAST_SEEN_INTERVAL {
            entry.1 = Instant::now();
            return true;
        }
        false
    }

    /// Drop every cached entry for an app.
    ///
    /// Called on revocation. Keyed by hash rather than by app id, so this has
    /// to scan — which is fine at this size and is the correct trade against
    /// carrying a second index purely for an operation that happens once per
    /// app deletion.
    pub fn invalidate(&self, app_id: &str) {
        self.entries.retain(|_, (identity, _)| identity.app_id != app_id);
    }
}

/// Last time an authenticated request arrived, as a Unix timestamp.
///
/// Feeds the idle-exit timer. Deliberately an atomic rather than a mutex: it
/// is written on the hot path of every authenticated request, and a lost
/// update under contention would at worst delay an idle shutdown by one tick.
#[derive(Debug, Default)]
pub struct ActivityClock(AtomicU64);

impl ActivityClock {
    pub fn new() -> Self {
        let clock = Self(AtomicU64::new(0));
        clock.touch();
        clock
    }

    pub fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.0.store(now, Ordering::Relaxed);
    }

    /// Seconds since the last authenticated request.
    pub fn idle_secs(&self) -> u64 {
        let last = self.0.load(Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(last)
    }
}

/// State for [`auth_middleware`].
#[derive(Clone)]
pub struct AuthState {
    pub pool: SqlitePool,
    pub cache: Arc<KeyCache>,
    /// Bootstrap credential authorizing `POST /api/apps/register` and nothing
    /// else. Regenerated every launch and published in the handshake file —
    /// see `bigtiny2_protocol::discovery` for what it does and does not
    /// protect against.
    pub registration_token: String,
    /// Bumped on every authenticated request; read by the idle-exit timer.
    pub activity: Arc<ActivityClock>,
}

/// Constant-time byte comparison: no early exit on the first differing byte,
/// so an attacker probing a token can't learn *where* two values diverge.
/// Loops over both inputs regardless of content; a length mismatch is folded
/// into the accumulator rather than returned early, so neither the length nor
/// the prefix is leaked through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len().max(b.len());
    let mut diff: u32 = (a.len() ^ b.len()) as u32;
    for i in 0..n {
        let ba = a.get(i).copied().unwrap_or(0) as u32;
        let bb = b.get(i).copied().unwrap_or(0) as u32;
        diff |= ba ^ bb;
    }
    diff == 0
}

/// Routes reachable without an app key.
///
/// `/api/health` is open so a client can poll readiness *and validate a
/// handshake* before it has registered — discovery depends on this. The
/// register route is open to app keys but gated on the registration token
/// instead, handled inside the middleware.
fn is_public(path: &str) -> bool {
    path == "/api/health"
}

fn is_registration(path: &str) -> bool {
    path == "/api/apps/register"
}

/// Resolve `X-API-Key` to an [`AppIdentity`] and attach it to the request.
pub async fn auth_middleware(
    State(auth): State<Arc<AuthState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    if !path.starts_with("/api") || is_public(&path) {
        return next.run(req).await;
    }

    // Registration is gated on the bootstrap token, not an app key — an app
    // has no key yet at this point, which is the whole reason it is calling.
    if is_registration(&path) {
        let presented = req
            .headers()
            .get("x-registration-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), auth.registration_token.as_bytes()) {
            return unauthorized("Missing or invalid X-Registration-Token");
        }
        return next.run(req).await;
    }

    let Some(key) = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return unauthorized("Missing or invalid X-API-Key");
    };

    let key_hash = apps::hash_key(&key);
    let identity = match auth.cache.get(&key_hash) {
        Some(identity) => identity,
        None => match apps::identity_for_key(&auth.pool, &key).await {
            Ok(Some(identity)) => {
                auth.cache.insert(key_hash.clone(), identity.clone());
                identity
            }
            Ok(None) => return unauthorized("Missing or invalid X-API-Key"),
            Err(e) => {
                // A database failure must not read as a valid key.
                tracing::error!("auth lookup failed: {e}");
                return unauthorized("Missing or invalid X-API-Key");
            }
        },
    };

    if auth.cache.should_touch(&key_hash) {
        let pool = auth.pool.clone();
        let app_id = identity.app_id.clone();
        // Fire-and-forget: liveness telemetry must never add latency to, or be
        // able to fail, the request that triggered it.
        tokio::spawn(async move {
            if let Err(e) = apps::touch_last_seen(&pool, &app_id).await {
                tracing::debug!("failed to record last_seen for {app_id}: {e}");
            }
        });
    }

    // Only authenticated traffic counts as activity. Health polling
    // deliberately does not: a client that merely watches the daemon
    // would otherwise keep it alive indefinitely, which defeats the point
    // of idle exit.
    auth.activity.touch();

    req.extensions_mut().insert(identity);
    next.run(req).await
}

fn unauthorized(detail: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "Unauthorized", "detail": detail})),
    )
        .into_response()
}

/// Logs `METHOD path -> status (duration_ms)` for every request.
pub async fn request_logging_middleware(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let start = Instant::now();

    let response = next.run(req).await;

    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(
        "{method} {path} -> {} ({duration_ms:.1}ms)",
        response.status().as_u16()
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::{Extension, Router};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn pool_with_app(app_id: &str, key: &str) -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        apps::register_app(&pool, app_id, app_id, key).await.unwrap();
        pool
    }

    fn app(pool: SqlitePool, registration_token: &str) -> Router {
        let state = Arc::new(AuthState {
            pool,
            cache: Arc::new(KeyCache::new()),
            registration_token: registration_token.to_string(),
            activity: Arc::new(ActivityClock::new()),
        });
        Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/apps/register", get(|| async { "registered" }))
            .route(
                "/api/chat/",
                // Echo the resolved app id so tests can assert *who* the
                // handler was told is calling, not merely that it ran.
                get(|Extension(id): Extension<AppIdentity>| async move { id.app_id }),
            )
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn get_req(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn health_stays_open_so_discovery_can_validate_a_handshake() {
        let pool = pool_with_app("kitty", "k").await;
        let resp = app(pool, "tok").oneshot(get_req("/api/health")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_missing_key_is_unauthorized() {
        let pool = pool_with_app("kitty", "k").await;
        let resp = app(pool, "tok").oneshot(get_req("/api/chat/")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unknown_key_is_unauthorized() {
        let pool = pool_with_app("kitty", "k").await;
        let req = Request::builder()
            .uri("/api/chat/")
            .header("x-api-key", "wrong")
            .body(Body::empty())
            .unwrap();
        let resp = app(pool, "tok").oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_valid_key_reaches_the_handler_as_a_named_identity() {
        // The core V2 property: success carries a name, so a handler can scope
        // its query. V1 could only answer "allowed".
        let pool = pool_with_app("kitty", "k").await;
        let req = Request::builder()
            .uri("/api/chat/")
            .header("x-api-key", "k")
            .body(Body::empty())
            .unwrap();
        let resp = app(pool, "tok").oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"kitty");
    }

    #[tokio::test]
    async fn two_apps_are_told_apart() {
        let pool = pool_with_app("kitty", "key-a").await;
        apps::register_app(&pool, "notebook", "Notebook", "key-b")
            .await
            .unwrap();

        for (key, expected) in [("key-a", "kitty"), ("key-b", "notebook")] {
            let req = Request::builder()
                .uri("/api/chat/")
                .header("x-api-key", key)
                .body(Body::empty())
                .unwrap();
            let resp = app(pool.clone(), "tok").oneshot(req).await.unwrap();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], expected.as_bytes());
        }
    }

    #[tokio::test]
    async fn registration_requires_the_bootstrap_token_not_an_app_key() {
        let pool = pool_with_app("kitty", "k").await;

        let no_token = app(pool.clone(), "tok")
            .oneshot(get_req("/api/apps/register"))
            .await
            .unwrap();
        assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);

        // An app key must NOT substitute for the registration token.
        let wrong = Request::builder()
            .uri("/api/apps/register")
            .header("x-registration-token", "k")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app(pool.clone(), "tok").oneshot(wrong).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let right = Request::builder()
            .uri("/api/apps/register")
            .header("x-registration-token", "tok")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app(pool, "tok").oneshot(right).await.unwrap().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn revocation_invalidates_the_cache_immediately() {
        // A deleted app must not keep working until a TTL expires — that
        // window is not acceptable for a revoked credential.
        let pool = pool_with_app("kitty", "k").await;
        let state = Arc::new(AuthState {
            pool: pool.clone(),
            cache: Arc::new(KeyCache::new()),
            registration_token: "tok".into(),
            activity: Arc::new(ActivityClock::new()),
        });
        let router = || {
            Router::new()
                .route("/api/chat/", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ))
        };
        let req = || {
            Request::builder()
                .uri("/api/chat/")
                .header("x-api-key", "k")
                .body(Body::empty())
                .unwrap()
        };

        // Prime the cache.
        assert_eq!(router().oneshot(req()).await.unwrap().status(), StatusCode::OK);

        apps::delete_app(&pool, "kitty").await.unwrap();
        state.cache.invalidate("kitty");

        assert_eq!(
            router().oneshot(req()).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn health_polling_does_not_count_as_activity() {
        // The property that makes idle exit work at all: a client that merely
        // watches the daemon must not keep it alive forever.
        let pool = pool_with_app("kitty", "k").await;
        let activity = Arc::new(ActivityClock::new());
        let state = Arc::new(AuthState {
            pool,
            cache: Arc::new(KeyCache::new()),
            registration_token: "tok".into(),
            activity: activity.clone(),
        });
        let router = || {
            Router::new()
                .route("/api/health", get(|| async { "ok" }))
                .route("/api/chat/", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ))
        };

        // Backdate the clock, then poll health: it must stay backdated.
        activity.0.store(1, Ordering::Relaxed);
        let _ = router().oneshot(get_req("/api/health")).await.unwrap();
        assert!(
            activity.idle_secs() > 1_000,
            "health polling must not reset the idle clock"
        );

        // An authenticated request does reset it.
        let req = Request::builder()
            .uri("/api/chat/")
            .header("x-api-key", "k")
            .body(Body::empty())
            .unwrap();
        let _ = router().oneshot(req).await.unwrap();
        assert!(activity.idle_secs() < 5);
    }

    #[tokio::test]
    async fn a_rejected_request_does_not_count_as_activity() {
        // Otherwise anyone able to reach the port could hold the daemon open
        // indefinitely with a stream of invalid keys.
        let pool = pool_with_app("kitty", "k").await;
        let activity = Arc::new(ActivityClock::new());
        let state = Arc::new(AuthState {
            pool,
            cache: Arc::new(KeyCache::new()),
            registration_token: "tok".into(),
            activity: activity.clone(),
        });
        let router = Router::new()
            .route("/api/chat/", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        activity.0.store(1, Ordering::Relaxed);
        let req = Request::builder()
            .uri("/api/chat/")
            .header("x-api-key", "wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(activity.idle_secs() > 1_000);
    }

    #[test]
    fn a_fresh_activity_clock_reads_as_just_used() {
        // Not zero-initialised: a daemon that has served nothing yet must not
        // look like it has been idle since 1970 and exit on its first tick.
        assert!(ActivityClock::new().idle_secs() < 5);
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
