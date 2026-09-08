//! Specialist definitions.
//!
//! # Scoping
//!
//! Two owners are possible and the difference matters at every call site:
//! `app_id IS NULL` is a built-in every app can see and none may modify, and a
//! set `app_id` is one app's own. An app's row *shadows* a built-in of the same
//! name for that app alone — so "edit the researcher" is a private copy, never
//! a rewrite of what other apps run.
//!
//! Every accessor a route may use is `*_for_app` and puts the owner in the
//! `WHERE` clause, matching `storage::sessions`. A specialist belonging to
//! another app reads as absent (404), not forbidden.

use serde_json::Value;
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;
use crate::models::specialist::Specialist;

#[derive(Debug, Clone, FromRow)]
pub struct SpecialistRow {
    pub id: String,
    pub app_id: Option<String>,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub tool_allow: String,
    pub response_schema: Option<String>,
    pub max_steps: i64,
    pub max_concurrent: Option<i64>,
    pub fan_out: Option<String>,
    pub reasoning_cap_tokens: Option<i64>,
    pub reasoning_cap_fraction: Option<f64>,
    pub enabled: i64,
    pub builtin: i64,
}

impl SpecialistRow {
    /// A malformed `tool_allow` yields an empty list, not everything.
    ///
    /// The failure has to bias toward fewer tools: a row corrupted into
    /// "unrestricted" would hand a delegate the app's whole surface, which is
    /// exactly the thing the column exists to prevent.
    pub fn into_model(self) -> Specialist {
        let tool_allow: Vec<String> =
            serde_json::from_str(&self.tool_allow).unwrap_or_else(|e| {
                tracing::warn!(
                    specialist = %self.name,
                    "tool_allow did not parse ({e}); treating as no tools"
                );
                Vec::new()
            });
        let response_schema: Option<Value> = self
            .response_schema
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        Specialist {
            id: self.id,
            app_id: self.app_id,
            name: self.name,
            description: self.description,
            system_prompt: self.system_prompt,
            provider: self.provider,
            model: self.model,
            tool_allow,
            response_schema,
            max_steps: self.max_steps,
            fan_out: self.fan_out,
            max_concurrent: self.max_concurrent,
            // An explicit token count outranks a fraction, matching the
            // configured-beats-derived rule used everywhere else (see
            // `provider::slots`). Both null means "daemon default".
            reasoning_cap: match (self.reasoning_cap_tokens, self.reasoning_cap_fraction) {
                (Some(n), _) => Some(crate::agent::tokens::ReasoningCap::Tokens(n as i32)),
                (None, Some(f)) => Some(crate::agent::tokens::ReasoningCap::ContextFraction(f)),
                (None, None) => None,
            },
            enabled: self.enabled != 0,
            builtin: self.builtin != 0,
        }
    }
}

/// The two cap columns, split back out of the enum. Exactly one is ever set,
/// which is what keeps "an explicit number" and "a share of the window"
/// distinguishable in the database rather than collapsing into one ambiguous
/// figure.
fn cap_tokens(spec: &Specialist) -> Option<i64> {
    match spec.reasoning_cap {
        Some(crate::agent::tokens::ReasoningCap::Tokens(n)) => Some(i64::from(n)),
        _ => None,
    }
}

fn cap_fraction(spec: &Specialist) -> Option<f64> {
    match spec.reasoning_cap {
        Some(crate::agent::tokens::ReasoningCap::ContextFraction(f)) => Some(f),
        _ => None,
    }
}

const COLUMNS: &str = "id, app_id, name, description, system_prompt, provider, model, \
                       tool_allow, response_schema, max_steps, max_concurrent, enabled, builtin, \
                       reasoning_cap_tokens, reasoning_cap_fraction, fan_out";

/// Every specialist visible to `app_id`: its own, plus the built-ins it has not
/// shadowed.
///
/// Shadowing is an explicit `NOT EXISTS` rather than a `GROUP BY name` over an
/// ordered subquery. The latter reads as if it takes the first row per group,
/// and SQLite promises no such thing for bare columns: it would work until it
/// silently did not, and the failure would be an app getting the built-in
/// researcher it thought it had replaced.
pub async fn list_visible(
    pool: &SqlitePool,
    app_id: &str,
) -> Result<Vec<Specialist>, StorageError> {
    let rows: Vec<SpecialistRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM specialists s \
         WHERE (s.app_id = ?1 OR s.app_id IS NULL) \
           AND (s.app_id IS NOT NULL \
                OR NOT EXISTS (SELECT 1 FROM specialists o \
                               WHERE o.name = s.name AND o.app_id = ?1)) \
         ORDER BY s.name"
    ))
    .bind(app_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(SpecialistRow::into_model).collect())
}

/// The definition `app_id` gets when it asks for `name` — its own if it has
/// one, otherwise the built-in. `None` when neither exists.
pub async fn resolve(
    pool: &SqlitePool,
    app_id: &str,
    name: &str,
) -> Result<Option<Specialist>, StorageError> {
    let row: Option<SpecialistRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM specialists \
         WHERE name = ? AND (app_id = ? OR app_id IS NULL) \
         ORDER BY (app_id IS NULL) LIMIT 1"
    ))
    .bind(name)
    .bind(app_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(SpecialistRow::into_model))
}

/// One row by id, regardless of owner. For routes that must tell "not yours"
/// (404) apart from "shared, and not yours to change" (403) — the same
/// distinction `routes::mcp` draws for a shared server.
pub async fn get_any(pool: &SqlitePool, id: &str) -> Result<Option<Specialist>, StorageError> {
    let row: Option<SpecialistRow> =
        sqlx::query_as(&format!("SELECT {COLUMNS} FROM specialists WHERE id = ?"))
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(SpecialistRow::into_model))
}

/// Insert or replace a definition.
///
/// `app_id` of `None` writes a built-in and is reachable only from
/// `specialists::registry::seed_builtins` — no route passes `None`.
pub async fn upsert(pool: &SqlitePool, spec: &Specialist) -> Result<(), StorageError> {
    let tool_allow = serde_json::to_string(&spec.tool_allow).unwrap_or_else(|_| "[]".into());
    let response_schema = spec
        .response_schema
        .as_ref()
        .map(|v| v.to_string());
    sqlx::query(
        "INSERT INTO specialists \
           (id, app_id, name, description, system_prompt, provider, model, tool_allow, \
            response_schema, max_steps, max_concurrent, enabled, builtin, \
            reasoning_cap_tokens, reasoning_cap_fraction, fan_out, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) \
         ON CONFLICT(COALESCE(app_id, ''), name) DO UPDATE SET \
           description = excluded.description, \
           system_prompt = excluded.system_prompt, \
           provider = excluded.provider, \
           model = excluded.model, \
           tool_allow = excluded.tool_allow, \
           response_schema = excluded.response_schema, \
           max_steps = excluded.max_steps, \
           max_concurrent = excluded.max_concurrent, \
           reasoning_cap_tokens = excluded.reasoning_cap_tokens, \
           reasoning_cap_fraction = excluded.reasoning_cap_fraction, \
           fan_out = excluded.fan_out, \
           enabled = excluded.enabled, \
           updated_at = datetime('now')",
    )
    .bind(&spec.id)
    .bind(&spec.app_id)
    .bind(&spec.name)
    .bind(&spec.description)
    .bind(&spec.system_prompt)
    .bind(&spec.provider)
    .bind(&spec.model)
    .bind(&tool_allow)
    .bind(&response_schema)
    .bind(spec.max_steps)
    .bind(spec.max_concurrent)
    .bind(i64::from(spec.enabled))
    .bind(i64::from(spec.builtin))
    .bind(cap_tokens(spec))
    .bind(cap_fraction(spec))
    .bind(&spec.fan_out)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seed a built-in only if no built-in of that name exists yet.
///
/// Distinct from `upsert` so a daemon restart cannot overwrite a built-in the
/// user has since edited in place. Returns whether a row was written.
pub async fn insert_builtin_if_absent(
    pool: &SqlitePool,
    spec: &Specialist,
) -> Result<bool, StorageError> {
    let tool_allow = serde_json::to_string(&spec.tool_allow).unwrap_or_else(|_| "[]".into());
    let response_schema = spec.response_schema.as_ref().map(|v| v.to_string());
    let out = sqlx::query(
        "INSERT INTO specialists \
           (id, app_id, name, description, system_prompt, provider, model, tool_allow, \
            response_schema, max_steps, max_concurrent, enabled, builtin, \
            reasoning_cap_tokens, reasoning_cap_fraction, fan_out) \
         VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 1, ?, ?, ?) \
         ON CONFLICT(COALESCE(app_id, ''), name) DO NOTHING",
    )
    .bind(&spec.id)
    .bind(&spec.name)
    .bind(&spec.description)
    .bind(&spec.system_prompt)
    .bind(&spec.provider)
    .bind(&spec.model)
    .bind(&tool_allow)
    .bind(&response_schema)
    .bind(spec.max_steps)
    .bind(spec.max_concurrent)
    .bind(cap_tokens(spec))
    .bind(cap_fraction(spec))
    .bind(&spec.fan_out)
    .execute(pool)
    .await?;
    Ok(out.rows_affected() > 0)
}

/// Delete an app's own specialist. Returns rows affected, so `0` reads as "no
/// such specialist" rather than an error — a built-in is never matched here,
/// which is what makes it undeletable.
pub async fn delete_for_app(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
) -> Result<u64, StorageError> {
    let out = sqlx::query("DELETE FROM specialists WHERE id = ? AND app_id = ?")
        .bind(id)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(out.rows_affected())
}
