//! Recipe CRUD + YAML import/export commands — mirrors
//! `commands::scheduled_tasks`'s shape exactly (thin persistence layer over
//! `config::recipes`/`config::recipe_yaml`, which own the actual data model
//! and validation logic).

use std::path::Path;

use tauri::{AppHandle, Emitter};

use crate::config;
use crate::config::recipe_yaml;
use crate::config::recipes::Recipe;
use crate::state::AppState;

fn emit_changed(app: &AppHandle) {
    let _ = app.emit("recipes://changed", ());
}

#[tauri::command]
pub fn list_recipes(state: tauri::State<'_, AppState>) -> Result<Vec<Recipe>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(cfg.recipes.clone())
}

/// Fields a caller supplies when creating/updating a recipe — everything
/// except the Kitty-only bookkeeping (`id`/`is_builtin`/`created_at`), which
/// these commands own.
#[derive(Debug, serde::Deserialize)]
pub struct RecipeInput {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub instructions: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub parameters: Vec<crate::config::recipes::RecipeParameter>,
    #[serde(default)]
    pub extensions: Vec<crate::config::recipes::RecipeExtension>,
    #[serde(default)]
    pub activities: Vec<String>,
    #[serde(default = "crate::config::recipes::default_max_reasoning_tokens")]
    pub max_reasoning_tokens: u32,
}

fn validate_input_and_slug(
    input: &RecipeInput,
    existing: &[Recipe],
    ignore_id: Option<&str>,
) -> Result<(), String> {
    if input.slug.trim().is_empty() {
        return Err("Slug can't be empty.".into());
    }
    if !input
        .slug
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase())
        || !input
            .slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(
            "Slug must start with a lowercase letter and contain only lowercase letters, digits, and underscores.".into(),
        );
    }
    if existing
        .iter()
        .any(|r| r.slug == input.slug && Some(r.id.as_str()) != ignore_id)
    {
        return Err(format!("Slug '{}' is already in use.", input.slug));
    }
    Ok(())
}

fn to_recipe(input: RecipeInput, id: String, is_builtin: bool, created_at: String) -> Recipe {
    Recipe {
        id,
        slug: input.slug,
        title: input.title,
        description: input.description,
        instructions: input.instructions,
        prompt: input.prompt,
        version: "1.0.0".to_string(),
        parameters: input.parameters,
        extensions: input.extensions,
        activities: input.activities,
        is_builtin,
        created_at,
        max_reasoning_tokens: input.max_reasoning_tokens,
    }
}

#[tauri::command]
pub fn create_recipe(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    recipe: RecipeInput,
) -> Result<Recipe, String> {
    let new_recipe = {
        let mut cfg = state.config.lock().unwrap();
        validate_input_and_slug(&recipe, &cfg.recipes, None)?;
        let id = format!("recipe_{}", chrono::Utc::now().timestamp_millis());
        let created_at = chrono::Utc::now().to_rfc3339();
        let new_recipe = to_recipe(recipe, id, false, created_at);
        let validation = recipe_yaml::validate_recipe(&new_recipe);
        if !validation.is_valid() {
            return Err(validation.errors.join(" "));
        }
        cfg.recipes.push(new_recipe.clone());
        config::save(&cfg).map_err(|e| e.to_string())?;
        new_recipe
    };
    emit_changed(&app);
    Ok(new_recipe)
}

#[tauri::command]
pub fn update_recipe(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    recipe: RecipeInput,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        let existing = cfg
            .recipes
            .iter()
            .find(|r| r.id == id)
            .ok_or("recipe not found")?;
        if existing.is_builtin {
            return Err(
                "Built-in templates can't be edited directly — use Duplicate as new recipe.".into(),
            );
        }
        validate_input_and_slug(&recipe, &cfg.recipes, Some(&id))?;
        let created_at = existing.created_at.clone();
        let updated = to_recipe(recipe, id.clone(), false, created_at);
        let validation = recipe_yaml::validate_recipe(&updated);
        if !validation.is_valid() {
            return Err(validation.errors.join(" "));
        }
        let slot = cfg.recipes.iter_mut().find(|r| r.id == id).unwrap();
        *slot = updated;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    emit_changed(&app);
    Ok(())
}

#[tauri::command]
pub fn delete_recipe(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        let existing = cfg
            .recipes
            .iter()
            .find(|r| r.id == id)
            .ok_or("recipe not found")?;
        if existing.is_builtin {
            return Err(
                "Built-in templates can't be deleted — use Duplicate as new recipe if you want your own editable copy."
                    .into(),
            );
        }
        cfg.recipes.retain(|r| r.id != id);
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    emit_changed(&app);
    Ok(())
}

#[tauri::command]
pub fn duplicate_recipe(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Recipe, String> {
    let new_recipe = {
        let mut cfg = state.config.lock().unwrap();
        let source = cfg
            .recipes
            .iter()
            .find(|r| r.id == id)
            .ok_or("recipe not found")?
            .clone();
        let mut copy = source.clone();
        copy.id = format!("recipe_{}", chrono::Utc::now().timestamp_millis());
        copy.title = format!("Copy of {}", source.title);
        copy.is_builtin = false;
        copy.created_at = chrono::Utc::now().to_rfc3339();
        copy.slug = unique_slug_from(&source.slug, &cfg.recipes);
        cfg.recipes.push(copy.clone());
        config::save(&cfg).map_err(|e| e.to_string())?;
        copy
    };
    emit_changed(&app);
    Ok(new_recipe)
}

fn unique_slug_from(base: &str, existing: &[Recipe]) -> String {
    let taken: std::collections::HashSet<&str> = existing.iter().map(|r| r.slug.as_str()).collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}_{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[derive(Debug, serde::Serialize)]
pub struct RecipeImportResult {
    pub recipe: Recipe,
    pub warnings: Vec<String>,
}

#[tauri::command]
pub fn import_recipe_yaml(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<RecipeImportResult, String> {
    let ext = Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext == "yml" {
        return Err(
            "Goose recipes use `.yaml`, not `.yml` — rename the file and try again.".into(),
        );
    }
    if ext != "yaml" && ext != "json" {
        return Err("Only .yaml or .json recipe files are supported.".into());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("could not read {path}: {e}"))?;

    let result = {
        let mut cfg = state.config.lock().unwrap();
        let id = format!("recipe_{}", chrono::Utc::now().timestamp_millis());
        let created_at = chrono::Utc::now().to_rfc3339();
        let import = recipe_yaml::parse_import(&text, &cfg.recipes, id, created_at)?;
        cfg.recipes.push(import.recipe.clone());
        config::save(&cfg).map_err(|e| e.to_string())?;
        RecipeImportResult {
            recipe: import.recipe,
            warnings: import.warnings,
        }
    };
    emit_changed(&app);
    Ok(result)
}

#[tauri::command]
pub fn export_recipe_yaml(
    state: tauri::State<'_, AppState>,
    id: String,
    path: String,
) -> Result<(), String> {
    let recipe = {
        let cfg = state.config.lock().unwrap();
        cfg.recipes
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or("recipe not found")?
    };
    let yaml = recipe_yaml::export_recipe(&recipe)?;
    std::fs::write(&path, yaml).map_err(|e| format!("could not write {path}: {e}"))
}
