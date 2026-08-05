//! Ports `plugins/bigtiny/bigtiny/server/middleware.py`'s three
//! `BaseHTTPMiddleware` classes as axum `from_fn`/`from_fn_with_state`
//! middleware. CORS and panic-catching (Python's `ErrorHandlingMiddleware`
//! equivalent — Rust handlers don't throw exceptions the way Python's do,
//! so the closest analogue is converting a handler panic into a 500 instead
//! of dropping the connection) are plain `tower_http` layers applied
//! alongside these in `lib.rs::run()`, not defined here.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Auth configuration threaded into `auth_middleware` as axum state.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub secret: Option<String>,
    /// When `true`, a missing secret is a misconfiguration to fail closed on
    /// rather than a signal to run unauthenticated. Desktop leaves this
    /// `false` (matches the historical Python "no secret -> no-op"
    /// behavior, appropriate for a single-user localhost daemon). An
    /// embedding host on a platform where loopback isn't process-private
    /// (Android: any app holding `INTERNET` can reach `127.0.0.1`) should
    /// set this `true` via `RunOptions::require_secret` so a missing secret
    /// denies every `/api/*` route instead of silently allowing all of them.
    pub required: bool,
}

/// Constant-time byte comparison: no early exit on the first differing byte,
/// so an attacker probing the `X-API-Key` header can't learn *where* two keys
/// diverge (a timing oracle against the old `==` short-circuit). Loops over
/// both inputs regardless of content; a length mismatch is folded into the
/// accumulator rather than returned early, so neither the length nor the
/// prefix is leaked through timing.
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

/// `X-API-Key` auth, matching `APIKeyMiddleware`'s shape: `/api/health`
/// always stays open (so launchers can poll readiness before auth is wired
/// up). Every other `/api/*` path's handling depends on `AuthConfig`:
/// - a configured secret always gates on an exact header match;
/// - no secret configured is a no-op *unless* `required` is set, in which
///   case it's treated as a misconfiguration and every non-health route is
///   denied — fail closed rather than fail open.
pub async fn auth_middleware(
    State(auth): State<Arc<AuthConfig>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let is_health = path == "/api/health";
    let is_api = path.starts_with("/api");

    if is_api && !is_health {
        match &auth.secret {
            Some(secret) => {
                let header_value = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
                // A missing header is unauthorized, exactly as before; a
                // present one is compared in constant time so the equality
                // check itself can't leak the secret through timing.
                let matches = header_value
                    .map(|v| constant_time_eq(v.as_bytes(), secret.as_bytes()))
                    .unwrap_or(false);
                if !matches {
                    return unauthorized();
                }
            }
            None if auth.required => return unauthorized(),
            None => {}
        }
    }

    next.run(req).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "Unauthorized", "detail": "Missing or invalid X-API-Key"})),
    )
        .into_response()
}

/// Logs `METHOD path -> status (duration_ms)` for every request, matching
/// `RequestLoggingMiddleware`.
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
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app_with_auth(auth: AuthConfig) -> Router {
        Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/chat/", get(|| async { "protected" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(auth),
                auth_middleware,
            ))
    }

    fn app_with_secret(secret: Option<&str>) -> Router {
        app_with_auth(AuthConfig {
            secret: secret.map(String::from),
            required: false,
        })
    }

    #[tokio::test]
    async fn health_stays_open_even_with_secret_configured() {
        let app = app_with_secret(Some("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_route_rejects_missing_key() {
        let app = app_with_secret(Some("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "Unauthorized");
        assert_eq!(body["detail"], "Missing or invalid X-API-Key");
    }

    #[tokio::test]
    async fn protected_route_rejects_wrong_key() {
        let app = app_with_secret(Some("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/")
                    .header("x-api-key", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_route_allows_correct_key() {
        let app = app_with_secret(Some("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/")
                    .header("x-api-key", "s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_secret_configured_is_a_noop() {
        let app = app_with_secret(None);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn required_with_no_secret_fails_closed_on_protected_routes() {
        let app = app_with_auth(AuthConfig {
            secret: None,
            required: true,
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn required_with_no_secret_still_leaves_health_open() {
        let app = app_with_auth(AuthConfig {
            secret: None,
            required: true,
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn required_with_secret_behaves_like_non_required() {
        let app = app_with_auth(AuthConfig {
            secret: Some("s3cret".to_string()),
            required: true,
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/")
                    .header("x-api-key", "s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
