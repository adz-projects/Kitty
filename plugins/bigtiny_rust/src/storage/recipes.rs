use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RecipeRow {
    pub id: String,
    pub name: String,
    pub prompt_template: String,
    pub instructions: Option<String>,
    pub parameters: Option<String>,
    pub required_mcp_servers: Option<String>,
    pub system_prompt_layer: Option<String>,
    pub max_steps: i32,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

pub async fn list_recipes(pool: &SqlitePool) -> Result<Vec<RecipeRow>, StorageError> {
    let rows = sqlx::query_as::<_, RecipeRow>(
        r#"SELECT id, name, prompt_template, instructions, parameters, required_mcp_servers,
                  system_prompt_layer, max_steps, created_at, updated_at
           FROM recipes ORDER BY name ASC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_recipe(
    pool: &SqlitePool,
    recipe_id: &str,
) -> Result<Option<RecipeRow>, StorageError> {
    let row = sqlx::query_as::<_, RecipeRow>(
        r#"SELECT id, name, prompt_template, instructions, parameters, required_mcp_servers,
                  system_prompt_layer, max_steps, created_at, updated_at
           FROM recipes WHERE id = ?"#,
    )
    .bind(recipe_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create_recipe(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    prompt_template: &str,
    instructions: Option<&str>,
    max_steps: i32,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"INSERT INTO recipes (id, name, prompt_template, instructions, max_steps)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(id)
    .bind(name)
    .bind(prompt_template)
    .bind(instructions)
    .bind(max_steps)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_recipe(
    pool: &SqlitePool,
    recipe_id: &str,
    name: Option<&str>,
    prompt_template: Option<&str>,
    instructions: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE recipes SET
           name = COALESCE(?1, name),
           prompt_template = COALESCE(?2, prompt_template),
           instructions = COALESCE(?3, instructions),
           updated_at = CURRENT_TIMESTAMP
           WHERE id = ?4"#,
    )
    .bind(name)
    .bind(prompt_template)
    .bind(instructions)
    .bind(recipe_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_recipe(pool: &SqlitePool, recipe_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM recipes WHERE id = ?"#)
        .bind(recipe_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
