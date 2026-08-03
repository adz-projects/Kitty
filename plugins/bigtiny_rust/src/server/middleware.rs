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

/// `X-API-Key` auth, matching `APIKeyMiddleware` exactly: `/api/health`
/// stays open (so launchers can poll readiness before auth is wired up);
/// every other `/api/*` path requires the header to equal the configured
/// secret. No secret configured -> no-op (matches Python: `self.secret and
/// ...`).
pub async fn auth_middleware(
    State(secret): State<Arc<Option<String>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let needs_auth = secret.is_some() && path.starts_with("/api") && path != "/api/health";

    if needs_auth {
        let header_value = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
        let matches = header_value
            .map(|v| Some(v) == secret.as_deref())
            .unwrap_or(false);
        if !matches {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Unauthorized", "detail": "Missing or invalid X-API-Key"})),
            )
                .into_response();
        }
    }

    next.run(req).await
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

    fn app_with_secret(secret: Option<&str>) -> Router {
        Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/chat/", get(|| async { "protected" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(secret.map(String::from)),
                auth_middleware,
            ))
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
}
