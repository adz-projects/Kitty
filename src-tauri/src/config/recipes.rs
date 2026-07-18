//! Goose recipes, reinterpreted as client-side templates rather than the real
//! Goose CLI's `goose run --recipe <path>` runner (that runner is a separate
//! process outside the shared `goose serve` Kitty talks to over ACP, and
//! `session/new` silently ignores any recipe/instructions param — confirmed
//! via a live probe, see `docs/acp-protocol.md`'s "Recipes / skills" section).
//! Instead, a recipe's `instructions`/`prompt`/`extensions` are applied to an
//! ordinary chat turn client-side (see `commands::session::add_recipe_extension`
//! and `chatStore.ts`'s `sendWithRecipe`), which trades away the real runner's
//! `response`-schema enforcement, `retry`, and `sub_recipes` subagent
//! delegation (out of scope for v1 — those need the actual CLI runner) for
//! full session/history/artifacts support with zero new process lifecycle.
//!
//! The struct shapes below intentionally mirror the real, portable Goose
//! recipe YAML schema (`docs/guides/recipes/recipe-reference` upstream) field
//! for field, so a recipe authored in Kitty round-trips cleanly through
//! `config::recipe_yaml`'s import/export as a real `.yaml` file usable by the
//! actual `goose run --recipe` CLI too — only `id`/`slug`/`is_builtin`/
//! `created_at` are Kitty-only bookkeeping, stripped on export.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Real Goose parameter `input_type` values. `String` is the schema default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterInputType {
    #[default]
    String,
    Number,
    Boolean,
    Date,
    File,
    Select,
}

/// `UserPrompt` is the one Kitty treats specially: at most one parameter per
/// recipe may carry it, and its value is whatever free text the invoker typed
/// after `/slug` (see `recipes::primaryParameter` on the TS side). This is a
/// direct reuse of the real schema's own semantics ("collect this
/// interactively"), not a Kitty-only concept needing special export handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterRequirement {
    Required,
    Optional,
    UserPrompt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeParameter {
    pub key: String,
    #[serde(default)]
    pub input_type: ParameterInputType,
    pub requirement: ParameterRequirement,
    /// For the one `user_prompt` parameter, this is user-facing invocation
    /// guidance shown in the slash-autocomplete dropdown and as the
    /// composer's placeholder text — not just schema metadata. Write it as a
    /// worked example of everything the recipe can use from the typed text,
    /// not just the topic (see the 4 built-in templates below).
    #[serde(default)]
    pub description: String,
    /// Required for every parameter except the `user_prompt` one and `file`
    /// type (forbidden for `file` per the real schema) — enforced by
    /// `recipe_yaml::validate_recipe`, not by the type system, since Kitty
    /// doesn't collect any parameter interactively except the primary one.
    #[serde(default)]
    pub default: Option<String>,
    /// Required (non-empty) when `input_type` is `Select`.
    #[serde(default)]
    pub options: Vec<String>,
}

/// Real Goose extension `type` values, kept as a plain string (validated
/// against this known set at import/save time by `recipe_yaml`) rather than
/// an enum, so an extension type Kitty can't launch still round-trips
/// losslessly through import → export instead of failing to parse.
pub const KNOWN_EXTENSION_TYPES: [&str; 6] = [
    "stdio",
    "builtin",
    "platform",
    "streamable_http",
    "frontend",
    "inline_python",
];

/// A recipe-declared extension. Only `builtin`/`platform`/`stdio` have an ACP
/// equivalent Kitty can actually add to a live session
/// (`commands::session::add_recipe_extension` maps `stdio` → the ACP `mcp`
/// shape); the rest are stored for YAML round-trip fidelity and silently
/// skipped at launch — never a hard failure, since an extension type ACP has
/// no representation for must not break a recipe invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeExtension {
    #[serde(rename = "type")]
    pub ext_type: String,
    pub name: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Names of env vars this extension needs; resolved to literal
    /// `KEY=VALUE` pairs from Kitty's own process env at launch time (never
    /// goosed's) — see `add_recipe_extension`.
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub bundled: Option<bool>,
    /// Any other real-schema fields (`uri`, `available_tools`, `code`, …)
    /// Kitty doesn't specifically interpret — preserved losslessly rather
    /// than modeled per-variant, so import → export never silently drops
    /// data from a recipe authored elsewhere.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    /// Stable, Kitty-internal identity — never shown to the user, never
    /// exported (see `recipe_yaml::export_recipe`).
    pub id: String,
    /// Invocation identifier after `/`, user-editable, unique among the
    /// user's own recipes. Not part of the real Goose schema — Kitty-only,
    /// stripped on export.
    pub slug: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub parameters: Vec<RecipeParameter>,
    #[serde(default)]
    pub extensions: Vec<RecipeExtension>,
    #[serde(default)]
    pub activities: Vec<String>,
    /// The 4 shipped templates only. Not editable/deletable in place — the
    /// editor offers "Duplicate as new recipe" instead (see
    /// `commands::recipes::update_recipe`/`delete_recipe`'s guard).
    #[serde(default)]
    pub is_builtin: bool,
    pub created_at: String,
    /// Hard cap on how much a recipe-invoked turn is allowed to reason before
    /// Kitty auto-cancels it (`chatStore.ts`'s `flushDeltas`). Kitty-only —
    /// not part of the real Goose recipe schema (there's no ACP-exposed
    /// numeric reasoning-token config to query per model; goosed's
    /// `session/new`/`session/load` `configOptions` only ever surface
    /// `thinking_effort` as an effort *level* — off/low/medium/high/max, no
    /// token count — confirmed via `docs/acp-protocol.md`'s live probe), so
    /// this is excluded from `recipe_yaml::export_recipe`'s output rather
    /// than forced into an otherwise-portable `.yaml`. Enforced client-side
    /// via a character-count approximation, since that's the only lever
    /// available without a real per-model token count to check against.
    #[serde(default = "default_max_reasoning_tokens")]
    pub max_reasoning_tokens: u32,
}

fn default_version() -> String {
    "1.0.0".to_string()
}

/// Fallback cap when no better number is available — which is always, today
/// (see `max_reasoning_tokens`'s doc comment). A conservative, "won't run
/// away" default rather than a precisely-researched one.
pub fn default_max_reasoning_tokens() -> u32 {
    2048
}

fn param(
    key: &str,
    requirement: ParameterRequirement,
    description: &str,
    default: Option<&str>,
) -> RecipeParameter {
    RecipeParameter {
        key: key.to_string(),
        input_type: ParameterInputType::String,
        requirement,
        description: description.to_string(),
        default: default.map(|s| s.to_string()),
        options: Vec::new(),
    }
}

fn select_param(key: &str, description: &str, options: &[&str], default: &str) -> RecipeParameter {
    RecipeParameter {
        key: key.to_string(),
        input_type: ParameterInputType::Select,
        requirement: ParameterRequirement::Optional,
        description: description.to_string(),
        default: Some(default.to_string()),
        options: options.iter().map(|s| s.to_string()).collect(),
    }
}

fn number_param(key: &str, description: &str, default: &str) -> RecipeParameter {
    RecipeParameter {
        key: key.to_string(),
        input_type: ParameterInputType::Number,
        requirement: ParameterRequirement::Optional,
        description: description.to_string(),
        default: Some(default.to_string()),
        options: Vec::new(),
    }
}

/// The 4 read-only templates shipped with the app. All declare zero
/// extensions — confirmed via `commands::session::new_session` (force-adds
/// the keyless `computercontroller` builtin, web/computer tools, to every
/// session) and `docs/acp-protocol.md` (the bundled `developer` platform
/// extension — file read/write — is `enabled: true` in a stock `config.yaml`
/// already), so instructions are written defensively rather than hard-
/// depending on a specific tool name. Fixed ids make re-seeding idempotent.
pub fn builtin_templates() -> Vec<Recipe> {
    vec![
        Recipe {
            id: "recipe_builtin_doc_creator".to_string(),
            slug: "documentation_creator".to_string(),
            title: "Documentation creator".to_string(),
            description: "Turns a codebase, folder, or rough notes into clear, well-organized documentation (README, API reference, or guide).".to_string(),
            instructions: Some(
                "You are a technical writer producing high-quality, accurate documentation. \
                Before writing anything, explore the working directory's actual files (using \
                whatever file-reading/shell tools you have available) to ground the \
                documentation in what really exists — never invent APIs, file names, or \
                behavior you haven't verified. Prefer clear structure: a short overview first, \
                then details organized under headings, with concrete examples (real function \
                signatures, real commands) rather than generic placeholders. Call out any \
                assumptions or gaps you had to guess at rather than silently presenting a guess \
                as fact. When you're done, save the documentation as a markdown file in the \
                working directory (pick a sensible filename based on what's being documented) \
                and tell the user where you saved it, in addition to showing the content inline."
                    .to_string(),
            ),
            prompt: Some(
                "Create {{doc_type}} documentation for: {{topic}}\n\nIntended audience: {{audience}}\n\nExplore the relevant code or notes first, then write the documentation."
                    .to_string(),
            ),
            version: default_version(),
            parameters: vec![
                param(
                    "topic",
                    ParameterRequirement::UserPrompt,
                    "What to document — a folder path, a feature name, or rough notes. \
                    Optionally add the doc type and audience (e.g. 'the auth module — API \
                    reference, for external integrators'); otherwise a README for general \
                    developers is assumed.",
                    None,
                ),
                select_param(
                    "doc_type",
                    "Kind of documentation to produce.",
                    &["readme", "api_reference", "guide", "architecture_overview"],
                    "readme",
                ),
                param(
                    "audience",
                    ParameterRequirement::Optional,
                    "Who this documentation is written for.",
                    Some("developers unfamiliar with this codebase"),
                ),
            ],
            extensions: Vec::new(),
            activities: vec![
                "Write a README for this project".to_string(),
                "Document the public API".to_string(),
                "message: Tip — point this at a folder path, a feature name, or paste rough notes to turn into structured docs.".to_string(),
            ],
            is_builtin: true,
            created_at: BUILTIN_CREATED_AT.to_string(),
            max_reasoning_tokens: default_max_reasoning_tokens(),
        },
        Recipe {
            id: "recipe_builtin_annotated_bibliography".to_string(),
            slug: "annotated_bibliography".to_string(),
            title: "Annotated bibliography researcher".to_string(),
            description: "Finds and summarizes scholarly and journalistic sources on a topic, producing a properly formatted annotated bibliography.".to_string(),
            instructions: Some(
                "You are a research assistant producing an annotated bibliography. For each \
                source: (1) find a real, verifiable source — a real paper, article, or report, \
                not a fabricated title or URL; (2) provide a full citation in the requested \
                style; (3) write a 3-5 sentence annotation summarizing the source's argument or \
                findings and noting its relevance, credibility, and any limitations (date, \
                methodology, potential bias). Prefer a mix of peer-reviewed/scholarly sources and \
                reputable journalistic coverage. If you have a live web search or fetch tool \
                available, use it to confirm sources are real and current; if you don't, say so \
                up front and clearly mark which sources are being recalled from training data \
                rather than freshly verified — it is better to flag uncertainty than to \
                fabricate a citation."
                    .to_string(),
            ),
            prompt: Some(
                "Find and annotate {{source_count}} sources on: {{topic}}\n\nCitation style: {{citation_style}}. Include a mix of scholarly and journalistic sources where available, and flag anything you can't independently verify."
                    .to_string(),
            ),
            version: default_version(),
            parameters: vec![
                param(
                    "topic",
                    ParameterRequirement::UserPrompt,
                    "The research topic or question — e.g. 'AI and cheating in higher education \
                    in 2026.' Optionally add a source count and citation style (e.g. '...2026 — \
                    10 sources, MLA'); otherwise 8 sources in APA are assumed.",
                    None,
                ),
                number_param("source_count", "How many sources to find.", "8"),
                select_param(
                    "citation_style",
                    "Citation format for each entry.",
                    &["apa", "mla", "chicago"],
                    "apa",
                ),
            ],
            extensions: Vec::new(),
            activities: vec![
                "Find scholarly sources on a topic".to_string(),
                "message: Best for research topics with a mix of academic and news coverage.".to_string(),
            ],
            is_builtin: true,
            created_at: BUILTIN_CREATED_AT.to_string(),
            max_reasoning_tokens: default_max_reasoning_tokens(),
        },
        Recipe {
            id: "recipe_builtin_debate_moderator".to_string(),
            slug: "debate_moderator".to_string(),
            title: "Multiturn debate moderator".to_string(),
            description: "Runs a structured, multi-round debate between two opposing positions on a topic, then moderates and summarizes.".to_string(),
            instructions: Some(
                "You are moderating a structured debate, playing both sides plus a neutral \
                moderator role. First, parse out any debater personas or stances the user \
                specified and adopt them faithfully for the whole debate — if none were given, \
                choose reasonable opposing positions yourself. State the motion clearly and \
                briefly define terms. Then run the requested number of rounds — in each round, \
                present the strongest good-faith argument FOR, then the strongest good-faith \
                argument AGAINST, each grounded in real reasoning rather than strawmen and \
                labeled by persona name when personas were given; rebuttals in later rounds must \
                engage the actual prior point, not restate the opening. After all rounds, \
                moderate: summarize the strongest points on each side, note where they genuinely \
                disagree versus where it's a values/framing difference, and close with a fair, \
                non-preachy synthesis — do not declare a \"winner\" unless explicitly asked to. \
                Label each turn clearly (e.g. \"FOR — Round 1\", \"AGAINST — Round 1\", \
                \"Moderator summary\") so it's easy to follow."
                    .to_string(),
            ),
            prompt: Some(
                "Motion and any debater personas: {{motion}}\n\nRun a {{rounds}}-round debate. Additional angles to cover: {{perspective_hint}}"
                    .to_string(),
            ),
            version: default_version(),
            parameters: vec![
                param(
                    "motion",
                    ParameterRequirement::UserPrompt,
                    "The debate motion, and — this is the part worth including — each debater's \
                    persona or stance, e.g. 'Should AI-generated art be copyrightable? — Debater \
                    A: a working illustrator; Debater B: an AI startup founder.' Personas are \
                    what make the debate specific; without them, reasonable opposing positions \
                    are chosen automatically.",
                    None,
                ),
                number_param("rounds", "How many rounds to run.", "3"),
                param(
                    "perspective_hint",
                    ParameterRequirement::Optional,
                    "Additional angles worth covering.",
                    Some("(use your judgment on which angles are most important)"),
                ),
            ],
            extensions: Vec::new(),
            activities: vec![
                "Debate a policy proposal".to_string(),
                "Debate a product decision".to_string(),
                "message: Great for stress-testing a decision from both sides before committing.".to_string(),
            ],
            is_builtin: true,
            created_at: BUILTIN_CREATED_AT.to_string(),
            max_reasoning_tokens: default_max_reasoning_tokens(),
        },
        Recipe {
            id: "recipe_builtin_public_document_analyzer".to_string(),
            slug: "public_document_analyzer".to_string(),
            title: "Public document analyzer".to_string(),
            description: "Analyzes a public document (a law, policy, report, contract, or filing) and produces a plain-language summary plus a critical read of its implications.".to_string(),
            instructions: Some(
                "You are analyzing a public document (legislation, policy, corporate filing, \
                contract, or report) for someone who needs to actually understand it, not just \
                skim jargon. If given a file path or dropped file, read it directly with your \
                file tools; if given a URL, fetch it if you have a fetch/browsing tool, otherwise \
                ask the user to paste the relevant text. Structure your analysis as: (1) a \
                plain-language summary of what the document actually says or does, calibrated to \
                the requested reading level; (2) the specific provisions or claims most relevant \
                to the requested focus; (3) a critical read — ambiguities, what's notably absent, \
                who benefits or is burdened, and any claims that seem inconsistent with the \
                document's own text; (4) open questions a careful reader should still ask. Quote \
                the document directly (short excerpts) when citing a specific provision, so the \
                user can verify you're not paraphrasing away nuance. Don't editorialize with a \
                political conclusion — analyze the mechanics and let the user draw their own."
                    .to_string(),
            ),
            prompt: Some(
                "Analyze this document: {{document}}\n\nFocus: {{focus}}\nReading level: {{reading_level}}"
                    .to_string(),
            ),
            version: default_version(),
            parameters: vec![
                param(
                    "document",
                    ParameterRequirement::UserPrompt,
                    "A path, URL, or pasted excerpt of the document. Optionally add what to \
                    focus on and the reading level (e.g. '...contract.pdf — focus on termination \
                    clauses, for a legal expert'); otherwise a general-audience overall summary \
                    is assumed.",
                    None,
                ),
                param(
                    "focus",
                    ParameterRequirement::Optional,
                    "What to focus the analysis on.",
                    Some("overall summary and key implications"),
                ),
                select_param(
                    "reading_level",
                    "How technical the analysis should read.",
                    &["general_public", "policy_professional", "legal_expert"],
                    "general_public",
                ),
            ],
            extensions: Vec::new(),
            activities: vec![
                "Analyze a piece of legislation".to_string(),
                "Analyze a contract or terms of service".to_string(),
                "message: Drop a file or paste a URL/excerpt as the document.".to_string(),
            ],
            is_builtin: true,
            created_at: BUILTIN_CREATED_AT.to_string(),
            max_reasoning_tokens: default_max_reasoning_tokens(),
        },
    ]
}

/// Fixed timestamp for the shipped templates — real creation time doesn't
/// matter for a built-in, and a fixed value keeps `builtin_templates()`
/// deterministic (useful for tests and idempotent re-seeding).
const BUILTIN_CREATED_AT: &str = "2026-01-01T00:00:00Z";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_templates_are_well_formed() {
        let templates = builtin_templates();
        assert_eq!(templates.len(), 4);
        for r in &templates {
            assert!(r.is_builtin);
            assert!(!r.slug.is_empty());
            assert!(!r.title.is_empty());
            assert!(r.instructions.is_some() || r.prompt.is_some());
            let primary_count = r
                .parameters
                .iter()
                .filter(|p| p.requirement == ParameterRequirement::UserPrompt)
                .count();
            assert_eq!(
                primary_count, 1,
                "recipe {} needs exactly one user_prompt parameter",
                r.slug
            );
            for p in &r.parameters {
                if p.requirement != ParameterRequirement::UserPrompt {
                    assert!(
                        p.default.is_some(),
                        "non-primary parameter {} on {} needs a default",
                        p.key,
                        r.slug
                    );
                }
            }
        }
        let slugs: Vec<_> = templates.iter().map(|r| r.slug.as_str()).collect();
        assert!(slugs.contains(&"annotated_bibliography"));
    }

    #[test]
    fn recipe_round_trips_through_json() {
        let r = builtin_templates().into_iter().next().unwrap();
        let text = serde_json::to_string(&r).unwrap();
        let back: Recipe = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, r.id);
        assert_eq!(back.parameters.len(), r.parameters.len());
    }

    #[test]
    fn recipe_extension_preserves_unknown_fields() {
        let json = r#"{"type":"stdio","name":"custom","cmd":"node","args":[],"env_keys":[],"available_tools":["a","b"]}"#;
        let ext: RecipeExtension = serde_json::from_str(json).unwrap();
        assert_eq!(
            ext.extra
                .get("available_tools")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let back = serde_json::to_string(&ext).unwrap();
        assert!(back.contains("available_tools"));
    }
}
