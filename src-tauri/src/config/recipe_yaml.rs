//! Validation plus YAML import/export for `Recipe`, keeping it a real,
//! portable Goose recipe (`docs/guides/recipes/recipe-reference` upstream
//! schema) as far as `title`/`description`/`instructions`/`prompt`/`version`/
//! `parameters`/`extensions`/`activities` go — only `id`/`slug`/`is_builtin`/
//! `created_at` are Kitty-only, stripped on export. Uses `serde_norway`
//! (`serde_yaml`'s actively-maintained continuation) rather than a hand
//! parser, since the real schema needs full YAML fidelity, not just a
//! subset.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::recipes::{
    default_max_reasoning_tokens, ParameterInputType, ParameterRequirement, Recipe,
    RecipeExtension, RecipeParameter, KNOWN_EXTENSION_TYPES,
};

/// Hand-rolled `{{ ... }}` scanner — a single trivial parse, not worth a new
/// `regex` dependency for (this codebase's existing minimal-dependency bias).
/// Returns each referenced variable name once per occurrence (duplicates
/// collapsed by the caller where that matters).
pub fn extract_template_vars(text: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = text[i + 2..].find("}}") {
                let inner = text[i + 2..i + 2 + end].trim();
                if !inner.is_empty() {
                    vars.push(inner.to_string());
                }
                i += 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    vars
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecipeValidation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl RecipeValidation {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Hard errors block save/import; warnings don't (surfaced to the user, but
/// the recipe is still usable — e.g. an unresolvable-at-invocation-time
/// parameter just shows a "needs attention" badge, computed the same way
/// client-side in `src/lib/recipes.ts`'s `recipeNeedsAttention`).
pub fn validate_recipe(r: &Recipe) -> RecipeValidation {
    let mut v = RecipeValidation::default();

    if r.title.trim().is_empty() {
        v.errors.push("Title is required.".to_string());
    }
    if r.description.trim().is_empty() {
        v.errors.push("Description is required.".to_string());
    }
    let has_instructions = r
        .instructions
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    let has_prompt = r.prompt.as_deref().is_some_and(|s| !s.trim().is_empty());
    if !has_instructions && !has_prompt {
        v.errors
            .push("At least one of Instructions or Prompt is required.".to_string());
    }

    let mut seen_keys = HashSet::new();
    let mut user_prompt_count = 0;
    for p in &r.parameters {
        if !seen_keys.insert(p.key.clone()) {
            v.errors
                .push(format!("Duplicate parameter key '{}'.", p.key));
        }
        if p.requirement == ParameterRequirement::UserPrompt {
            user_prompt_count += 1;
        }
        if p.input_type == ParameterInputType::Select && p.options.is_empty() {
            v.errors.push(format!(
                "Parameter '{}' is a select but declares no options.",
                p.key
            ));
        }
        if p.input_type == ParameterInputType::File && p.default.is_some() {
            v.errors.push(format!(
                "Parameter '{}' is a file parameter — it can't have a default.",
                p.key
            ));
        }
        let needs_default = p.requirement == ParameterRequirement::Optional
            || (p.requirement == ParameterRequirement::Required
                && p.input_type != ParameterInputType::File);
        if p.requirement != ParameterRequirement::UserPrompt
            && needs_default
            && p.default.as_deref().unwrap_or("").is_empty()
        {
            v.warnings.push(format!(
                "Parameter '{}' has no default and isn't bound to the slash command's typed \
                text — Kitty can't collect it interactively, so it will always resolve empty.",
                p.key
            ));
        }
    }
    if user_prompt_count > 1 {
        v.errors.push(
            "Only one parameter can be marked as bound to the slash command's typed text."
                .to_string(),
        );
    }

    for ext in &r.extensions {
        if !KNOWN_EXTENSION_TYPES.contains(&ext.ext_type.as_str()) {
            v.errors.push(format!(
                "Unknown extension type '{}' (expected one of: {}).",
                ext.ext_type,
                KNOWN_EXTENSION_TYPES.join(", ")
            ));
        }
    }

    let mut referenced = HashSet::new();
    for text in [
        r.instructions.as_deref().unwrap_or(""),
        r.prompt.as_deref().unwrap_or(""),
    ] {
        for var in extract_template_vars(text) {
            referenced.insert(var);
        }
    }
    for act in &r.activities {
        for var in extract_template_vars(act) {
            referenced.insert(var.clone());
        }
    }
    for key in &referenced {
        if !seen_keys.contains(key) {
            v.warnings.push(format!(
                "'{{{{{key}}}}}' is referenced but no parameter with that key is declared."
            ));
        }
    }
    for p in &r.parameters {
        if !referenced.contains(&p.key) {
            v.warnings.push(format!(
                "Parameter '{}' is declared but never referenced in Instructions, Prompt, or \
                Activities.",
                p.key
            ));
        }
    }

    v
}

/// The real, portable schema fields only — no `id`/`slug`/`is_builtin`/
/// `created_at`. Field order here is what gets written to the exported YAML.
#[derive(Debug, Serialize, Deserialize)]
struct RecipeSchema {
    title: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parameters: Vec<RecipeParameter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extensions: Vec<RecipeExtension>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    activities: Vec<String>,
}

/// Real-schema keys Kitty can't apply (needs the actual `goose run --recipe`
/// CLI runner) — surfaced as an import warning naming what each does, rather
/// than silently dropped or hard-rejected.
const UNSUPPORTED_SCHEMA_KEYS: [(&str, &str); 4] = [
    (
        "settings",
        "a model/provider/temperature override — Kitty always uses the currently active provider",
    ),
    (
        "response",
        "a structured JSON response schema — only enforced by the real Goose CLI recipe runner",
    ),
    (
        "retry",
        "automated retry-on-validation-failure — only supported by the real Goose CLI recipe runner",
    ),
    (
        "sub_recipes",
        "subagent delegation to child recipes — only supported by the real Goose CLI recipe runner",
    ),
];

fn derive_slug(title: &str, existing: &[Recipe]) -> String {
    let mut slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    let slug = slug.trim_matches('_');
    let slug = if slug.is_empty() { "recipe" } else { slug };
    let slug = slug
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .to_string();
    let slug = if slug.is_empty() {
        "recipe".to_string()
    } else {
        slug
    };

    let taken: HashSet<&str> = existing.iter().map(|r| r.slug.as_str()).collect();
    if !taken.contains(slug.as_str()) {
        return slug;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{slug}_{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[derive(Debug)]
pub struct ImportResult {
    pub recipe: Recipe,
    pub warnings: Vec<String>,
}

/// Parses YAML/JSON text into a `Recipe`. `.yml` should be rejected by the
/// caller before this is reached (Goose's own docs: only `.yaml`/`.json` are
/// supported). Hard schema errors abort with a clear message; unsupported
/// real-schema keys and soft validation issues come back as warnings
/// alongside the successfully-parsed recipe.
pub fn parse_import(
    text: &str,
    existing: &[Recipe],
    id: String,
    created_at: String,
) -> Result<ImportResult, String> {
    let raw: serde_norway::Value =
        serde_norway::from_str(text).map_err(|e| format!("Not valid YAML/JSON: {e}"))?;

    let mut warnings = Vec::new();
    if let Some(map) = raw.as_mapping() {
        for (key, note) in UNSUPPORTED_SCHEMA_KEYS {
            if map.contains_key(key) {
                warnings.push(format!(
                    "This recipe declares a `{key}` block ({note}) — it will be ignored."
                ));
            }
        }
    }

    let schema: RecipeSchema = serde_norway::from_value(raw)
        .map_err(|e| format!("Doesn't match the recipe schema: {e}"))?;

    let slug = derive_slug(&schema.title, existing);

    let recipe = Recipe {
        id,
        slug,
        title: schema.title,
        description: schema.description,
        instructions: schema.instructions,
        prompt: schema.prompt,
        version: schema.version,
        parameters: schema.parameters,
        extensions: schema.extensions,
        activities: schema.activities,
        is_builtin: false,
        created_at,
        // Not part of the real schema (see `Recipe::max_reasoning_tokens`'s
        // doc comment) — an imported recipe gets the same default a
        // brand-new one does.
        max_reasoning_tokens: default_max_reasoning_tokens(),
    };

    let validation = validate_recipe(&recipe);
    if !validation.is_valid() {
        return Err(validation.errors.join(" "));
    }
    warnings.extend(validation.warnings);

    Ok(ImportResult { recipe, warnings })
}

/// Serializes only the real, portable schema fields as YAML — no
/// `id`/`slug`/`is_builtin`/`created_at`.
pub fn export_recipe(r: &Recipe) -> Result<String, String> {
    let schema = RecipeSchema {
        title: r.title.clone(),
        description: r.description.clone(),
        instructions: r.instructions.clone(),
        prompt: r.prompt.clone(),
        version: r.version.clone(),
        parameters: r.parameters.clone(),
        extensions: r.extensions.clone(),
        activities: r.activities.clone(),
    };
    serde_norway::to_string(&schema).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::recipes::builtin_templates;

    #[test]
    fn extract_template_vars_finds_all_occurrences() {
        let vars = extract_template_vars("Hello {{ name }}, your {{topic}} is ready. {{name}}.");
        assert_eq!(vars, vec!["name", "topic", "name"]);
    }

    #[test]
    fn extract_template_vars_ignores_unclosed_and_empty() {
        assert_eq!(extract_template_vars("no vars here"), Vec::<String>::new());
        assert_eq!(
            extract_template_vars("unclosed {{ oops"),
            Vec::<String>::new()
        );
        assert_eq!(
            extract_template_vars("empty {{}} here"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn validate_recipe_requires_title_and_description() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        r.title = String::new();
        r.description = String::new();
        let v = validate_recipe(&r);
        assert!(!v.is_valid());
        assert!(v.errors.iter().any(|e| e.contains("Title")));
        assert!(v.errors.iter().any(|e| e.contains("Description")));
    }

    #[test]
    fn validate_recipe_requires_instructions_or_prompt() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        r.instructions = None;
        r.prompt = None;
        let v = validate_recipe(&r);
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("Instructions or Prompt")));
    }

    #[test]
    fn validate_recipe_rejects_duplicate_parameter_keys() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        let dup = r.parameters[0].clone();
        r.parameters.push(dup);
        let v = validate_recipe(&r);
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("Duplicate parameter key")));
    }

    #[test]
    fn validate_recipe_rejects_select_without_options() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        r.parameters.push(RecipeParameter {
            key: "style".to_string(),
            input_type: ParameterInputType::Select,
            requirement: ParameterRequirement::Optional,
            description: String::new(),
            default: Some("x".to_string()),
            options: Vec::new(),
        });
        let v = validate_recipe(&r);
        assert!(v.errors.iter().any(|e| e.contains("declares no options")));
    }

    #[test]
    fn validate_recipe_rejects_more_than_one_user_prompt_parameter() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        let mut second_primary = r.parameters[0].clone();
        second_primary.key = "other".to_string();
        r.parameters.push(second_primary);
        let v = validate_recipe(&r);
        assert!(v.errors.iter().any(|e| e.contains("Only one parameter")));
    }

    #[test]
    fn validate_recipe_warns_on_unresolvable_required_parameter() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        r.parameters.push(RecipeParameter {
            key: "cant_resolve".to_string(),
            input_type: ParameterInputType::String,
            requirement: ParameterRequirement::Required,
            description: String::new(),
            default: None,
            options: Vec::new(),
        });
        let v = validate_recipe(&r);
        assert!(v.is_valid());
        assert!(v.warnings.iter().any(|w| w.contains("cant_resolve")));
    }

    #[test]
    fn validate_recipe_rejects_unknown_extension_type() {
        let mut r = builtin_templates().into_iter().next().unwrap();
        r.extensions.push(RecipeExtension {
            ext_type: "totally_made_up".to_string(),
            name: "x".to_string(),
            cmd: None,
            args: Vec::new(),
            env_keys: Vec::new(),
            description: None,
            timeout: None,
            bundled: None,
            extra: Default::default(),
        });
        let v = validate_recipe(&r);
        assert!(v
            .errors
            .iter()
            .any(|e| e.contains("Unknown extension type")));
    }

    #[test]
    fn export_then_import_round_trips() {
        let original = builtin_templates().into_iter().next().unwrap();
        let yaml = export_recipe(&original).unwrap();
        assert!(!yaml.contains("is_builtin"));
        assert!(!yaml.contains("slug"));
        let result = parse_import(&yaml, &[], "new_id".to_string(), "now".to_string()).unwrap();
        assert_eq!(result.recipe.title, original.title);
        assert_eq!(result.recipe.description, original.description);
        assert_eq!(result.recipe.parameters.len(), original.parameters.len());
        assert!(!result.recipe.is_builtin);
        assert_eq!(result.recipe.slug, original.slug); // same title -> same derived slug
    }

    #[test]
    fn export_excludes_max_reasoning_tokens_and_import_defaults_it() {
        // Kitty-only safety field (see `Recipe::max_reasoning_tokens`'s doc
        // comment) — not part of the real, portable schema, so it must not
        // leak into an exported .yaml a real `goose run --recipe` might read;
        // a recipe imported from elsewhere gets the same default a brand-new
        // one does, not a garbage/missing value.
        let mut original = builtin_templates().into_iter().next().unwrap();
        original.max_reasoning_tokens = 9999; // a deliberately non-default value
        let yaml = export_recipe(&original).unwrap();
        assert!(!yaml.contains("max_reasoning_tokens"));
        assert!(!yaml.contains("9999"));
        let result = parse_import(&yaml, &[], "new_id".to_string(), "now".to_string()).unwrap();
        assert_eq!(
            result.recipe.max_reasoning_tokens,
            default_max_reasoning_tokens()
        );
    }

    #[test]
    fn import_warns_about_unsupported_settings_block() {
        let yaml = "title: Test\ndescription: A test recipe\nprompt: hi\nversion: \"1.0.0\"\nsettings:\n  goose_model: claude-sonnet-4\n";
        let result = parse_import(yaml, &[], "id".to_string(), "now".to_string()).unwrap();
        assert!(result.warnings.iter().any(|w| w.contains("settings")));
    }

    #[test]
    fn import_rejects_recipe_missing_both_instructions_and_prompt() {
        let yaml = "title: Test\ndescription: A test recipe\nversion: \"1.0.0\"\n";
        let err = parse_import(yaml, &[], "id".to_string(), "now".to_string()).unwrap_err();
        assert!(err.contains("Instructions or Prompt"));
    }

    #[test]
    fn derive_slug_dedupes_against_existing() {
        let mut existing = builtin_templates();
        // Same title twice -> same derived base slug -> must be suffixed.
        let first = derive_slug("My Custom Recipe", &existing);
        assert_eq!(first, "my_custom_recipe");
        let mut taken = existing[0].clone();
        taken.slug = first;
        existing.push(taken);
        let second = derive_slug("My Custom Recipe", &existing);
        assert_eq!(second, "my_custom_recipe_2");
    }
}
