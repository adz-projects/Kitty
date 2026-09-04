use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::agent::Agent;
use crate::error::RecipeError;
use crate::mcp::MCPManager;
use crate::storage::recipes::{self, RecipeRow};

/// Ports `plugins/bigtiny/bigtiny/recipes/engine.py::RecipeEngine`: renders a
/// recipe's `prompt_template`/`instructions` (Jinja — `minijinja` here, a
/// pure-Rust Jinja2 subset, rather than hand-rolling `{{var}}`-only
/// substitution which would silently diverge on any recipe using
/// `{% if %}`/`{% for %}`), creates a session carrying the rendered
/// instructions as `persona_override`, best-effort-connects the recipe's
/// `required_mcp_servers` by name, and runs the turn to completion.
pub struct RecipeEngine {
    db: SqlitePool,
    agent: Arc<Agent>,
    mcp: Arc<MCPManager>,
    recipes_dir: PathBuf,
}

impl RecipeEngine {
    pub fn new(
        db: SqlitePool,
        agent: Arc<Agent>,
        mcp: Arc<MCPManager>,
        recipes_dir: impl AsRef<Path>,
    ) -> Self {
        Self {
            db,
            agent,
            mcp,
            recipes_dir: recipes_dir.as_ref().to_path_buf(),
        }
    }

    /// Load `*.yaml`/`*.yml` recipe files from `directory` (or the
    /// configured default) into the `recipes` table via upsert-by-id.
    /// Malformed files are skipped with a warning, matching Python.
    pub async fn load_recipes_from_directory(&self, directory: Option<&Path>) -> usize {
        let target = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.recipes_dir.clone());
        if !target.is_dir() {
            tracing::warn!("Recipe directory not found: {}", target.display());
            return 0;
        }

        let mut count = 0;
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&target) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|e| e.to_str()),
                        Some("yaml") | Some("yml")
                    )
                })
                .collect(),
            Err(e) => {
                tracing::warn!(
                    "Failed to read recipe directory {}: {}",
                    target.display(),
                    e
                );
                return 0;
            }
        };
        entries.sort();

        for fpath in entries {
            match self.load_one_recipe_file(&fpath).await {
                Ok(()) => count += 1,
                Err(e) => tracing::warn!("Failed to load recipe {}: {}", fpath.display(), e),
            }
        }
        count
    }

    async fn load_one_recipe_file(&self, fpath: &Path) -> Result<(), String> {
        let raw = std::fs::read_to_string(fpath).map_err(|e| e.to_string())?;
        let data: Value = serde_yaml::from_str(&raw).map_err(|e| e.to_string())?;
        let Some(obj) = data.as_object() else {
            return Ok(()); // empty/non-mapping file: silently skip, matching Python
        };

        let stem = fpath
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("recipe");
        // Falls back to the file's stem, not a fresh random id — a random id
        // on every load meant an id-less YAML file could never match its own
        // previous row on reload (the `ON CONFLICT(id)` upsert below always
        // missed), creating a duplicate recipe entry each time.
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| stem.to_string());
        let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or(stem);
        let prompt_template = obj
            .get("prompt_template")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let instructions = obj.get("instructions").and_then(|v| v.as_str());
        let parameters = obj.get("parameters").cloned().unwrap_or_else(|| json!([]));
        let required_servers = obj
            .get("required_mcp_servers")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let system_prompt_layer = obj.get("system_prompt_layer").and_then(|v| v.as_str());
        let max_steps = obj.get("max_steps").and_then(|v| v.as_i64()).unwrap_or(30);

        sqlx::query(
            r#"INSERT INTO recipes (id, name, prompt_template, instructions, parameters, required_mcp_servers, system_prompt_layer, max_steps)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 prompt_template = excluded.prompt_template,
                 instructions = excluded.instructions,
                 parameters = excluded.parameters,
                 required_mcp_servers = excluded.required_mcp_servers,
                 system_prompt_layer = excluded.system_prompt_layer,
                 max_steps = excluded.max_steps,
                 updated_at = CURRENT_TIMESTAMP"#,
        )
        .bind(&id)
        .bind(name)
        .bind(prompt_template)
        .bind(instructions)
        .bind(parameters.to_string())
        .bind(required_servers.to_string())
        .bind(system_prompt_layer)
        .bind(max_steps)
        .execute(&self.db)
        .await
        .map_err(|e| e.to_string())?;

        Ok(())
    }

    /// Render the recipe, create a session for it, best-effort-connect its
    /// required MCP servers, and run the turn to completion — returns the
    /// new session's id.
    pub async fn execute(&self, recipe_id: &str, parameters: Value) -> Result<String, RecipeError> {
        let recipe: RecipeRow = recipes::get_recipe(&self.db, recipe_id)
            .await?
            .ok_or_else(|| RecipeError::NotFound(recipe_id.to_string()))?;

        let prompt = render_template(&recipe.prompt_template, &parameters)?;
        let instructions = recipe
            .instructions
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|t| render_template(t, &parameters))
            .transpose()?;

        // The agent reads `persona_override` from session metadata, so the
        // recipe's instructions/system_prompt_layer must land there to take
        // effect — matching Python's rationale exactly.
        let mut persona_parts: Vec<String> = Vec::new();
        if let Some(instructions) = &instructions {
            persona_parts.push(instructions.clone());
        }
        if let Some(layer) = &recipe.system_prompt_layer {
            if !layer.is_empty() {
                persona_parts.push(format!("You are a {layer}."));
            }
        }

        let mut metadata = json!({
            "recipe_id": recipe_id,
            "parameters": parameters,
            "recipe_name": recipe.name,
        });
        if !persona_parts.is_empty() {
            metadata["persona_override"] = json!(persona_parts.join("\n"));
        }

        let session_id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query("INSERT INTO sessions (id, name, metadata) VALUES (?1, ?2, ?3)")
            .bind(&session_id)
            .bind(&recipe.name)
            .bind(metadata.to_string())
            .execute(&self.db)
            .await
            .map_err(crate::error::StorageError::from)?;

        let required_servers: Vec<String> = recipe
            .required_mcp_servers
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        for server_name in &required_servers {
            if let Some(server_id) = self.find_server_id_by_name(server_name).await {
                if let Err(e) = self.mcp.connect_server(&server_id).await {
                    tracing::warn!("Failed to connect MCP server '{server_name}': {e}");
                }
            }
        }

        // Propagate the turn's outcome: `run_turn_and_wait` reports a
        // terminal `Error` frame as `Err`, so a provider-failed run surfaces
        // here instead of being recorded as a successful execution.
        self.agent
            .run_turn_and_wait(&session_id, &prompt)
            .await
            .map_err(RecipeError::TurnFailed)?;

        Ok(session_id)
    }

    async fn find_server_id_by_name(&self, name: &str) -> Option<String> {
        sqlx::query("SELECT id FROM mcp_servers WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
            .map(|row| row.get::<String, _>("id"))
    }
}

fn render_template(template: &str, params: &Value) -> Result<String, RecipeError> {
    let env = minijinja::Environment::new();
    env.render_str(template, params)
        .map_err(|e| RecipeError::Template(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_template_substitutes_variables() {
        let params = json!({"name": "world"});
        let out = render_template("Hello {{ name }}!", &params).unwrap();
        assert_eq!(out, "Hello world!");
    }

    #[test]
    fn render_template_supports_control_flow() {
        let params = json!({"items": ["a", "b"]});
        let out = render_template("{% for i in items %}{{ i }}{% endfor %}", &params).unwrap();
        assert_eq!(out, "ab");
    }
}
