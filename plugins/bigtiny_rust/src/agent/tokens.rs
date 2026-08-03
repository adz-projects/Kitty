use once_cell::sync::Lazy;
use serde_json::Value;

/// Real tokenizer, not a heuristic — `tiktoken-rs` was already a declared
/// dependency (chosen specifically for accurate, fast token counting) but
/// was never actually wired up; this module used a bare `bytes / 4`
/// estimate instead. That estimate is systematically wrong for anything
/// that isn't plain English prose (CJK text, code, JSON, dense punctuation),
/// and every consumer of this count is a real budget: compaction
/// thresholds, the live-tail cap, and the emergency-trim valve that's
/// supposed to guarantee a request never exceeds the model's actual context
/// window. An inaccurate count there isn't just imprecise bookkeeping — it
/// can under-trim right up to (or past) a real API-level context-length
/// error, which the trimming logic exists specifically to prevent.
static ENCODING: Lazy<Option<tiktoken_rs::CoreBPE>> = Lazy::new(|| tiktoken_rs::cl100k_base().ok());

/// Count tokens for plain text content using the real `cl100k_base`
/// tokenizer (falls back to the old byte-based heuristic only if the
/// embedded encoding data somehow fails to load, which should never happen
/// in practice — better a rough estimate than a panic).
pub fn count_text_tokens(text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    match ENCODING.as_ref() {
        Some(enc) => enc.encode_ordinary(text).len() as i32,
        None => text.len() as i32 / 4,
    }
}

/// Token count for one context message, matching what actually gets serialized.
pub fn count_message_tokens(msg: &Value) -> i32 {
    let mut total = 0;

    if let Some(content) = msg.get("content") {
        if let Some(text) = content.as_str() {
            total += count_text_tokens(text);
        } else if let Some(blocks) = content.as_array() {
            for block in blocks {
                if let Some(block) = block.as_object() {
                    if let Some(block_type) = block.get("type").and_then(|t| t.as_str()) {
                        if block_type == "text" {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                total += count_text_tokens(text);
                            }
                        } else if block_type == "image" {
                            total += 256;
                        }
                    }
                }
            }
        }
    }

    if let Some(tool_calls) = msg.get("tool_calls") {
        total += count_text_tokens(&serde_json::to_string(tool_calls).unwrap_or_default());
    }

    if let Some(tool_call_id) = msg.get("tool_call_id") {
        total += count_text_tokens(&tool_call_id.to_string());
    }

    // Small fixed overhead per message for role/framing tokens
    total + 4
}

/// Token count for a list of messages.
pub fn count_messages_tokens(messages: &[Value]) -> i32 {
    messages.iter().map(count_message_tokens).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_count_text_tokens_empty() {
        assert_eq!(count_text_tokens(""), 0);
    }

    #[test]
    fn test_count_text_tokens_basic() {
        // Real cl100k_base encoding of this phrase is 6 tokens (each word
        // happens to be its own token) — coincidentally the same number the
        // old `bytes/4` heuristic produced for this particular string, but
        // this now reflects the actual tokenizer, not an approximation.
        let tokens = count_text_tokens("Hello world this is a test");
        assert_eq!(tokens, 6);
    }

    #[test]
    fn test_count_text_tokens_cjk_is_not_underestimated() {
        // The old `bytes/4` heuristic significantly undercounts CJK text:
        // 3 bytes/char / 4 ≈ 0.75 "tokens" per character, while cl100k_base
        // typically spends close to one full token per CJK character —
        // real encoding must come out noticeably higher than the old
        // heuristic would have, not lower.
        let text = "你好世界这是一个测试"; // 10 CJK characters, 30 bytes
        let byte_heuristic = text.len() as i32 / 4; // what the old code returned
        let real = count_text_tokens(text);
        assert!(
            real > byte_heuristic,
            "expected real tokenizer ({real}) to exceed the old byte-based heuristic ({byte_heuristic})"
        );
    }

    #[test]
    fn test_count_message_tokens_simple() {
        let msg = json!({
            "role": "user",
            "content": "Hello"
        });
        let tokens = count_message_tokens(&msg);
        // "Hello" = 1 real token + 4 fixed per-message overhead = 5
        assert!(tokens >= 4);
    }

    #[test]
    fn test_count_message_tokens_with_tool_calls() {
        let msg = json!({
            "role": "assistant",
            "content": "Let me check",
            "tool_calls": [
                {
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\": \"/test.txt\"}"
                    }
                }
            ]
        });
        let tokens = count_message_tokens(&msg);
        // Should include content + tool_calls JSON + overhead
        assert!(tokens > 8);
    }

    #[test]
    fn test_count_messages_tokens() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
        ];
        let total = count_messages_tokens(&messages);
        assert!(total > 0);
    }
}
