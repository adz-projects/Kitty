//! Feature detection for reasoning/thinking-capable models. Rust port of
//! `src/lib/reasoning_models.ts` (the frontend's Phase 10 thinking-indicator
//! heuristic) — same name-pattern table, same "never assume, re-verify on
//! model updates" caveat recorded in `docs/VERSIONS.md`. Used here to gate
//! thought-seeding (`agent/loop_.rs::pathway_recall`): seeding a `<think>`
//! prefill into a model that doesn't actually have a thinking phase would
//! leak the seed's raw framing into the visible answer, so this must never
//! be assumed from provider type alone.
//!
//! Two independent copies (this one and the TS original) is a real
//! duplication-drift risk — if the pattern table changes, both need
//! updating. Accepted for now since the frontend and daemon are separate
//! deployable artifacts with no shared-code mechanism between them; revisit
//! if a third consumer appears.

use once_cell::sync::Lazy;
use regex::Regex;

static REASONING_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)think",     // lfm2.5-thinking, qwen3-thinking, *-thinking
        r"(?i)reason",
        r"(?i)deepseek-?r1",
        r"(?i)\bqwq\b",
        r"(?i)magistral",
        r"(?i)\bo[1-4](-|\b)", // OpenAI o-series (o1/o3/o4)
        r"(?i)\br1\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("static reasoning-model pattern must compile"))
    .collect()
});

/// True if the model name suggests it streams a distinct reasoning trace.
pub fn supports_reasoning(model: &str) -> bool {
    if model.trim().is_empty() {
        return false;
    }
    REASONING_PATTERNS.iter().any(|re| re.is_match(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_blank_is_false() {
        assert!(!supports_reasoning(""));
        assert!(!supports_reasoning("   "));
    }

    #[test]
    fn matches_known_reasoning_models() {
        for m in [
            "lfm2.5-thinking",
            "qwen3-thinking:4b",
            "deepseek-r1:8b",
            "deepseek r1",
            "qwq:32b",
            "magistral-small",
            "o1-preview",
            "o3-mini",
            "o4",
            "some-r1-variant",
        ] {
            assert!(supports_reasoning(m), "expected {m} to match");
        }
    }

    #[test]
    fn does_not_match_ordinary_models() {
        for m in ["llama3.2:3b", "qwen2.5-coder:7b", "gpt-4o", "claude-sonnet-4-20250514"] {
            assert!(!supports_reasoning(m), "expected {m} to NOT match");
        }
    }

    #[test]
    fn case_insensitive() {
        assert!(supports_reasoning("QWEN3-THINKING"));
        assert!(supports_reasoning("DeepSeek-R1"));
    }
}
