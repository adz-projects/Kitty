//! Shared ACP bookkeeping types used by both [`crate::goosed::api`] (the
//! request/response client) and [`crate::goosed::stream`] (incoming-frame
//! dispatch). Extracted to a leaf module so neither of those two depends on
//! the other for these definitions — `stream` previously imported them from
//! `api`, while `api` imports `stream` to hand off incoming frames, which is
//! a real cycle at the module-graph level.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::{oneshot, Mutex};

pub type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>;
/// Deferred `session/request_permission` requests: tool-call id -> JSON-RPC id
/// to respond to once the user approves/denies.
pub type Perm = Arc<Mutex<HashMap<String, Value>>>;
/// Last time a `session/update` notification arrived for a given session id —
/// lets `request_session_prompt`'s idle-reset timeout tell "actively streaming,
/// just slow" apart from "genuinely hung" (see its doc comment).
pub type Activity = Arc<Mutex<HashMap<String, Instant>>>;
/// In-flight tool calls: `toolCallId -> (toolName, extensionName)`, populated
/// on the initial `tool_call` notification (which carries the name but no
/// status) and consumed on the completing `tool_call_update` (which carries
/// status but not the name) — lets the adaptive-pathway auto-record-outcome
/// backstop (see `stream::emit_session_update`) know what just ran without
/// depending on the model calling `record_outcome` itself.
pub type ToolCalls = Arc<Mutex<HashMap<String, (String, String)>>>;
