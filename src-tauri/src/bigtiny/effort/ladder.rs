//! A model's reasoning-effort ladder: what it offers, in what order, which
//! level a new chat starts on, and how each level maps to the star picker.
//!
//! Pure throughout — no I/O, no `AppHandle` — so every rule here is unit
//! tested without a network.

use serde::{Deserialize, Serialize};

/// The wire value for "reasoning off". The engine also accepts `none`, the
/// spelling OpenAI and OpenRouter use, but Kitty always sends this one.
pub const OFF: &str = "off";

/// The single level of a toggle-only model (reasoning on, no grade).
pub const ON: &str = "on";

/// Where a card's ladder came from — shown in Settings beside its age.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortSource {
    /// The provider's own models API (Anthropic's `capabilities`).
    ProviderApi,
    /// OpenRouter's catalog `reasoning` object.
    OpenRouter,
    /// A self-hosted server's chat template (`/props`).
    ChatTemplate,
    /// The model's HuggingFace `tokenizer_config.json` chat template.
    HuggingFace,
    /// Ollama's `/api/show` `capabilities`.
    Ollama,
    /// Kitty's built-in name patterns — the last resort.
    Heuristic,
}

impl EffortSource {
    pub fn label(self) -> &'static str {
        match self {
            EffortSource::ProviderApi => "the provider",
            EffortSource::OpenRouter => "OpenRouter",
            EffortSource::ChatTemplate => "the server's chat template",
            EffortSource::HuggingFace => "HuggingFace",
            EffortSource::Ollama => "Ollama",
            EffortSource::Heuristic => "the model name",
        }
    }
}

/// What one card's model offers for reasoning effort, as detected.
///
/// `levels` is ascending (least reasoning first) and never contains Off;
/// `can_disable` says whether Off is offered on top. Empty `levels` with
/// `can_disable == false` means the model has no control at all and the
/// picker is hidden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffortCaps {
    pub levels: Vec<String>,
    pub can_disable: bool,
    /// Anthropic cards only: `"adaptive"` (current models — `thinking:
    /// adaptive` + `output_config.effort`) or `"budget"` (older
    /// `budget_tokens` models). Relayed to the engine on the provider row.
    #[serde(default)]
    pub anthropic_thinking: Option<String>,
    pub source: EffortSource,
    /// Unix seconds.
    pub detected_at: i64,
    /// Set when the engine reported the model rejected a level — the next
    /// resolution re-detects even though the entry is young.
    #[serde(default)]
    pub stale: bool,
}

impl EffortCaps {
    pub fn has_control(&self) -> bool {
        !self.levels.is_empty() || self.can_disable
    }
}

/// Rank of a well-known level; unknown levels return `None` and keep their
/// position relative to their neighbours.
fn rank(level: &str) -> Option<u8> {
    Some(match level {
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => return None,
    })
}

/// Normalize a raw level list into the ascending ladder: lowercase, trimmed,
/// deduplicated, `off`/`none`/`on` removed (those are not grades), sorted by
/// [`rank`]. Sources disagree on direction — OpenRouter and Qwen templates
/// list highest first — so the known ranks decide; an unknown level is placed
/// after the known level that preceded it in the source, read low→high.
pub fn normalize_levels<S: AsRef<str>>(raw: &[S]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for l in raw {
        let l = l.as_ref().trim().to_ascii_lowercase();
        if l.is_empty() || l == OFF || l == "none" || l == ON || seen.contains(&l) {
            continue;
        }
        seen.push(l);
    }
    // Read the source in ascending order: if its known ranks mostly descend,
    // it was written highest first.
    let known: Vec<u8> = seen.iter().filter_map(|l| rank(l)).collect();
    let descending = known.windows(2).filter(|w| w[0] > w[1]).count()
        > known.windows(2).filter(|w| w[0] < w[1]).count();
    if descending {
        seen.reverse();
    }
    // Stable sort by the rank of the nearest known level at or before each
    // position, so unknown levels ride along with their predecessor.
    let mut carried = 0u8;
    let mut keyed: Vec<(u8, usize, String)> = seen
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let k = match rank(&l) {
                Some(r) => {
                    carried = r;
                    r * 2
                }
                None => carried * 2 + 1,
            };
            (k, i, l)
        })
        .collect();
    keyed.sort_by_key(|(k, i, _)| (*k, *i));
    keyed.into_iter().map(|(_, _, l)| l).collect()
}

/// The level a new chat starts on: `medium` when the model offers it,
/// otherwise the middle of the ladder (the lower of the two on an even count —
/// the cheaper choice when there is no true middle). `None` when the ladder is
/// empty, i.e. the model can only be switched off.
pub fn default_level(levels: &[String]) -> Option<String> {
    if levels.iter().any(|l| l == "medium") {
        return Some("medium".to_string());
    }
    if levels.is_empty() {
        return None;
    }
    Some(levels[(levels.len() - 1) / 2].clone())
}

/// A human label for a level. Known levels get a friendly name; an unknown
/// one is title-cased so a novel model level still reads sanely.
pub fn pretty_level(level: &str) -> String {
    match level {
        OFF | "none" => "Off".to_string(),
        ON => "On".to_string(),
        "minimal" => "Minimal".to_string(),
        "low" => "Low".to_string(),
        "medium" => "Medium".to_string(),
        "high" => "High".to_string(),
        "xhigh" => "Extra high".to_string(),
        "max" => "Max".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => other.to_string(),
            }
        }
    }
}

/// One option for the picker: level *i* of an ascending ladder (counting
/// from 1) is drawn as *i* stars; Off is 0.
#[derive(Debug, Clone, PartialEq)]
pub struct LadderOption {
    pub value: String,
    pub name: String,
    pub stars: u8,
}

/// The full option list for a ladder, Off first when offered.
pub fn options(caps: &EffortCaps) -> Vec<LadderOption> {
    let mut out = Vec::with_capacity(caps.levels.len() + 1);
    if caps.can_disable {
        out.push(LadderOption {
            value: OFF.to_string(),
            name: pretty_level(OFF),
            stars: 0,
        });
    }
    for (i, l) in caps.levels.iter().enumerate() {
        out.push(LadderOption {
            value: l.clone(),
            name: pretty_level(l),
            stars: u8::try_from(i + 1).unwrap_or(u8::MAX),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(levels: &[&str]) -> Vec<String> {
        levels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn openrouter_ladders_are_reversed_and_lose_none() {
        assert_eq!(
            normalize_levels(&["max", "xhigh", "high", "medium", "low", "none"]),
            v(&["low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(
            normalize_levels(&["high", "minimal", "none"]),
            v(&["minimal", "high"])
        );
    }

    #[test]
    fn template_order_and_unknown_levels() {
        // The real Qwen3 guard, highest first.
        assert_eq!(
            normalize_levels(&["xhigh", "medium", "low"]),
            v(&["low", "medium", "xhigh"])
        );
        // An unknown level stays beside the known one it followed.
        assert_eq!(
            normalize_levels(&["low", "deep", "high"]),
            v(&["low", "deep", "high"])
        );
        assert_eq!(
            normalize_levels(&["Low", "low", " HIGH "]),
            v(&["low", "high"])
        );
    }

    #[test]
    fn default_is_medium_when_offered_else_the_middle() {
        // Current Claude ladder: medium, not the positional middle (high).
        assert_eq!(
            default_level(&v(&["low", "medium", "high", "xhigh", "max"])).as_deref(),
            Some("medium")
        );
        assert_eq!(
            default_level(&v(&["low", "high", "max"])).as_deref(),
            Some("high")
        );
        assert_eq!(
            default_level(&v(&["low", "medium", "xhigh"])).as_deref(),
            Some("medium")
        );
        assert_eq!(default_level(&v(&["high"])).as_deref(), Some("high"));
        // Even count, no medium: the lower middle.
        assert_eq!(
            default_level(&v(&["minimal", "high"])).as_deref(),
            Some("minimal")
        );
        assert_eq!(default_level(&v(&["on"])).as_deref(), Some("on"));
        assert_eq!(default_level(&[]), None);
    }

    #[test]
    fn options_map_levels_to_stars_with_off_first() {
        let caps = EffortCaps {
            levels: v(&["low", "medium", "high"]),
            can_disable: true,
            anthropic_thinking: None,
            source: EffortSource::OpenRouter,
            detected_at: 0,
            stale: false,
        };
        let opts = options(&caps);
        let got: Vec<(&str, u8)> = opts.iter().map(|o| (o.value.as_str(), o.stars)).collect();
        assert_eq!(
            got,
            vec![("off", 0), ("low", 1), ("medium", 2), ("high", 3)]
        );
        assert_eq!(opts[0].name, "Off");

        let mandatory = EffortCaps {
            can_disable: false,
            ..caps
        };
        assert_eq!(options(&mandatory)[0].value, "low");
    }

    #[test]
    fn pretty_level_names_are_readable() {
        assert_eq!(pretty_level("xhigh"), "Extra high");
        assert_eq!(pretty_level("on"), "On");
        assert_eq!(pretty_level("ultra"), "Ultra");
    }
}
