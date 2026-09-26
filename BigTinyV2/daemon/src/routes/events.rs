//! `GET /api/apps/me/events` -- the calling app's own event stream.
//!
//! Carries what an app needs to hear about when it has no turn stream open for
//! the session concerned: approvals a background chat or a scheduled run is
//! waiting on (`hitl_pause`/`hitl_resolved`), and scheduled runs starting and
//! finishing (`schedule_run`). See `server::app_events` for the hub behind it.
//!
//! Scoped strictly to the caller: an approval prompt shows another app's tool
//! arguments, which are that app's data.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::Extension;
use bytes::Bytes;
use tokio::sync::broadcast::error::RecvError;

use crate::server::events::serialize_sse;
use crate::storage::apps::AppIdentity;

use super::AppState;

/// How often an otherwise-silent SSE stream sends a comment line.
///
/// Two jobs. A client can hold an idle deadline without mistaking "nothing is
/// happening yet" (a turn waiting on an approval, an app with nothing to hear
/// about) for a dead connection; and the server learns a client has gone the
/// next time a write fails, rather than never, which is what lets a turn whose
/// client vanished mid-approval be cancelled instead of waiting it out.
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The comment frame sent on [`KEEPALIVE_INTERVAL`]. SSE clients ignore
/// comment lines, so it never reaches an application as an event.
pub const KEEPALIVE_FRAME: &[u8] = b": keepalive\n\n";

pub async fn events(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let mut rx = state.agent.app_events().subscribe();
    let app_id = identity.app_id;
    let stream = async_stream::stream! {
        // Sent at once, so a client knows the subscription is live before the
        // first real event -- and can then fetch anything it missed while
        // disconnected (pending approvals, run history) without a race.
        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b": subscribed\n\n"));
        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.tick().await; // the first tick completes immediately
        loop {
            tokio::select! {
                received = rx.recv() => match received {
                    Ok((owner, event)) if owner == app_id => {
                        yield Ok(Bytes::from(serialize_sse(&event)));
                    }
                    Ok(_) => {}
                    // A client this far behind has lost events; tell it, so it
                    // re-syncs from the REST endpoints instead of trusting a
                    // stream with a hole in it.
                    Err(RecvError::Lagged(n)) => {
                        yield Ok(Bytes::from(format!(": lagged {n}\n\n")));
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = keepalive.tick() => yield Ok(Bytes::from_static(KEEPALIVE_FRAME)),
            }
        }
    };
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert("Cache-Control", HeaderValue::from_static("no-cache"));
    response
}
