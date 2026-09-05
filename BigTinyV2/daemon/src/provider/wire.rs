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

use std::borrow::Cow;

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
pub fn sanitize_for_wire(messages: &[Value]) -> Cow<'_, [Value]> {
    // Rebuilding is a deep clone of the whole transcript -- every message
    // body, every tool result -- and it runs on each provider attempt of each
    // step. Most arrays need no rebuilding at all: the internal keys are
    // stamped by specific paths, so a message that has none is untouched by
    // definition. Scan first (a key comparison per top-level key, no
    // allocation), and only pay for the copy when there is something to
    // strip.
    let dirty = messages.iter().any(|msg| {
        msg.as_object()
            .is_some_and(|obj| obj.keys().any(|k| is_internal(k)))
    });
    if !dirty {
        return Cow::Borrowed(messages);
    }

    Cow::Owned(
        messages
            .iter()
            .map(|msg| match msg {
                // Only the messages that actually carry an internal key are
                // rebuilt; the rest are cloned as-is, which for a `Value` is
                // still a deep copy but avoids re-collecting a fresh map.
                Value::Object(obj) if obj.keys().any(|k| is_internal(k)) => Value::Object(
                    obj.iter()
                        .filter(|(k, _)| !is_internal(k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
                other => other.clone(),
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn database_identity_never_reaches_a_provider() {
        // The pre-existing leak: `id` is a row identifier, and it was being
        // sent to OpenAI and Anthropic on every turn since V1.
        let msgs = [json!({
            "id": "row-uuid",
            "rowid": 42,
            "role": "user",
            "content": "hello",
        })];
        let out = sanitize_for_wire(&msgs);
        assert_eq!(out[0], json!({"role": "user", "content": "hello"}));
    }

    #[test]
    fn underscore_prefixed_bookkeeping_is_stripped() {
        let msgs = [json!({
            "_tok": 12,
            "_future_internal_key": true,
            "role": "assistant",
            "content": "hi",
        })];
        let out = sanitize_for_wire(&msgs);
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
        let msgs = [msg.clone()];
        let out = sanitize_for_wire(&msgs);
        assert_eq!(out[0], msg, "no provider-visible field may be dropped");
    }

    #[test]
    fn a_tool_calls_nested_id_is_untouched() {
        // `id` inside a tool call is part of the wire format and load-bearing:
        // the provider matches results to calls by it. Only *top-level* keys
        // are internal.
        let msgs = [json!({
            "id": "row-uuid",
            "role": "assistant",
            "tool_calls": [{"id": "call_abc", "type": "function"}],
        })];
        let out = sanitize_for_wire(&msgs);
        assert!(out[0].get("id").is_none(), "the row id goes");
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_abc", "the call id stays");
    }

    #[test]
    fn non_object_entries_pass_through_unchanged() {
        let msgs = [json!("a bare string"), json!(null)];
        let out = sanitize_for_wire(&msgs);
        assert_eq!(out.as_ref(), &[json!("a bare string"), json!(null)]);
    }

    #[test]
    fn an_empty_array_stays_empty() {
        assert!(sanitize_for_wire(&[]).is_empty());
    }

    #[test]
    fn a_clean_array_is_not_copied_at_all() {
        // The common case, and the reason this is worth a fast path: nothing
        // to strip means nothing to allocate, on every attempt of every step.
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        assert!(matches!(sanitize_for_wire(&msgs), Cow::Borrowed(_)));
    }

    #[test]
    fn one_dirty_message_does_not_spare_the_array_but_still_strips_correctly() {
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"_tok": 3, "role": "assistant", "content": "hi"}),
        ];
        let out = sanitize_for_wire(&msgs);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(out[0], msgs[0], "a clean message survives unchanged");
        assert_eq!(out[1], json!({"role": "assistant", "content": "hi"}));
    }
}
