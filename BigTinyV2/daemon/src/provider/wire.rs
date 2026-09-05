//! Stripping daemon-internal bookkeeping off messages before they go out.
//!
//! # A pre-existing leak
//!
//! Messages reach a provider **verbatim**: `openai_compat::chat_completion`
//! puts the array straight into the request body and nothing filters keys. So
//! the `id` that `save_messages` stamps onto every in-memory message has been
//! going over the wire since V1 — a database row id, sent to OpenAI and
//! Anthropic on every turn.
//!
//! In practice both tolerate unknown fields in a message object, which is why
//! it was never noticed. It is still the daemon leaking its internals into a
//! third party's request log, and a stricter endpoint would reject it.
//!
//! # Why it matters now
//!
//! Phase 6 adds a second internal key (`_tok`, the token-count hint), which
//! makes an explicit sanitation step necessary rather than merely tidy. Fixing
//! `id` at the same time costs nothing.
//!
//! One place, both dialects, with a test that fails if anything internal
//! reaches a request body.

use serde_json::Value;

/// Keys the daemon uses internally and no provider should ever see.
///
/// `rowid` and `id` are database identity; anything `_`-prefixed is per-turn
/// bookkeeping. The prefix rule means a future internal key is covered without
/// anyone having to remember this file exists.
fn is_internal(key: &str) -> bool {
    key.starts_with('_') || key == "id" || key == "rowid"
}

/// Strip internal keys from every message.
///
/// Applied once, at the point every request funnels through, rather than at
/// each of the places a message is built — there are many of the latter and
/// exactly one of the former.
pub fn sanitize_for_wire(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| match msg {
            Value::Object(obj) => Value::Object(
                obj.iter()
                    .filter(|(k, _)| !is_internal(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            other => other.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn database_identity_never_reaches_a_provider() {
        // The pre-existing leak: `id` is a row identifier, and it was being
        // sent to OpenAI and Anthropic on every turn since V1.
        let out = sanitize_for_wire(&[json!({
            "id": "row-uuid",
            "rowid": 42,
            "role": "user",
            "content": "hello",
        })]);
        assert_eq!(out[0], json!({"role": "user", "content": "hello"}));
    }

    #[test]
    fn underscore_prefixed_bookkeeping_is_stripped() {
        let out = sanitize_for_wire(&[json!({
            "_tok": 12,
            "_future_internal_key": true,
            "role": "assistant",
            "content": "hi",
        })]);
        assert_eq!(out[0], json!({"role": "assistant", "content": "hi"}));
    }

    #[test]
    fn everything_a_provider_needs_survives() {
        // Over-stripping would be worse than under-stripping: a dropped
        // `tool_calls` breaks the conversation rather than merely leaking.
        let msg = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "call_1", "type": "function",
                            "function": {"name": "read", "arguments": {}}}],
            "tool_call_id": "call_1",
            "name": "read",
        });
        let out = sanitize_for_wire(&[msg.clone()]);
        assert_eq!(out[0], msg, "no provider-visible field may be dropped");
    }

    #[test]
    fn a_tool_calls_nested_id_is_untouched() {
        // `id` inside a tool call is part of the wire format and load-bearing:
        // the provider matches results to calls by it. Only *top-level* keys
        // are internal.
        let out = sanitize_for_wire(&[json!({
            "id": "row-uuid",
            "role": "assistant",
            "tool_calls": [{"id": "call_abc", "type": "function"}],
        })]);
        assert!(out[0].get("id").is_none(), "the row id goes");
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_abc", "the call id stays");
    }

    #[test]
    fn non_object_entries_pass_through_unchanged() {
        let out = sanitize_for_wire(&[json!("a bare string"), json!(null)]);
        assert_eq!(out, vec![json!("a bare string"), json!(null)]);
    }

    #[test]
    fn an_empty_array_stays_empty() {
        assert!(sanitize_for_wire(&[]).is_empty());
    }
}
