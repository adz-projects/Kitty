//! The `apps` table: registered clients and their keys.
//!
//! This is the row every other table's `app_id` points at, and the reason V2
//! can host several frontends at once where V1 could host exactly one.
//!
//! # Why keys are stored, not generated per launch
//!
//! V1 generated a fresh `BIGTINY_SECRET` on every daemon start and handed it to
//! the one process that spawned the daemon. That is free when the spawner and
//! the sole client are the same process. With several apps it breaks: an app
//! that did not spawn the daemon has no way to learn a per-launch secret, and
//! every daemon restart would force every app to re-register -- which it can
//! only do if it happens to still be running.
//!
//! So keys are long-lived and survive restarts. What is *not* stored is the key
//! itself: only a SHA-256 of it, so a leaked database yields no usable
//! credentials. The plaintext is returned exactly once, at registration, and
//! the app persists it in its own secret store.

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::error::StorageError;

/// A registered client, as resolved from an `X-API-Key` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    pub app_id: String,
    pub scopes: Vec<String>,
}

impl AppIdentity {
    /// Whether this app holds `scope`, with `"*"` meaning "everything".
    ///
    /// Scopes are stored but not yet enforced anywhere beyond this helper --
    /// every app registers with `["*"]` today. The column exists now because
    /// adding it later would mean a migration plus an auth change on a live
    /// multi-app daemon, and the shape is cheap to carry in the meantime.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == "*" || s == scope)
    }
}

/// A registered app's stored row.
#[derive(Debug, Clone)]
pub struct AppRow {
    pub id: String,
    pub display_name: String,
    pub scopes: Vec<String>,
    pub default_provider_id: Option<String>,
    pub default_model: Option<String>,
    pub created_at: Option<String>,
    pub last_seen_at: Option<String>,
}

/// Hash a key for storage and lookup.
///
/// A plain SHA-256 rather than a password KDF, deliberately: these are
/// high-entropy machine-generated tokens (see `generate_api_key`), not
/// user-chosen passwords, so there is no dictionary to slow down -- and this
/// runs on the auth path of every single request, where a deliberately slow
/// hash would be a self-inflicted bottleneck. The lookup is also cached (see
/// `server::middleware`), but must stay cheap for the cold path.
pub fn hash_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Register a new app and return its plaintext key.
///
/// Returns `Ok(None)` when `app_id` is already taken. That is a 409, not an
/// error to log: it is the ordinary outcome of an app that lost its stored key
/// trying to register again, and the caller needs to tell those apart from a
/// genuine failure.
pub async fn register_app(
    pool: &SqlitePool,
    app_id: &str,
    display_name: &str,
    api_key: &str,
) -> Result<Option<String>, StorageError> {
    let key_hash = hash_key(api_key);
    let result = sqlx::query(
        "INSERT OR IGNORE INTO apps (id, display_name, key_hash, created_at) \
         VALUES (?, ?, ?, datetime('now'))",
    )
    .bind(app_id)
    .bind(display_name)
    .bind(&key_hash)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Ok(None);
    }
    Ok(Some(api_key.to_string()))
}

/// Resolve a presented key to an identity, or `None` if it matches no app.
pub async fn identity_for_key(
    pool: &SqlitePool,
    api_key: &str,
) -> Result<Option<AppIdentity>, StorageError> {
    let key_hash = hash_key(api_key);
    let row = sqlx::query("SELECT id, scopes FROM apps WHERE key_hash = ?")
        .bind(&key_hash)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| AppIdentity {
        app_id: r.get::<String, _>("id"),
        scopes: parse_scopes(r.get::<String, _>("scopes").as_str()),
    }))
}

/// A malformed `scopes` blob degrades to no scopes rather than to full access.
///
/// Fail closed: the alternative -- treating unparseable JSON as `["*"]` --
/// would turn a corrupt row into a privilege escalation.
fn parse_scopes(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

pub async fn get_app(pool: &SqlitePool, app_id: &str) -> Result<Option<AppRow>, StorageError> {
    let row = sqlx::query(
        "SELECT id, display_name, scopes, default_provider_id, default_model, \
                created_at, last_seen_at \
         FROM apps WHERE id = ?",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| AppRow {
        id: r.get("id"),
        display_name: r.get("display_name"),
        scopes: parse_scopes(r.get::<String, _>("scopes").as_str()),
        default_provider_id: r.get("default_provider_id"),
        default_model: r.get("default_model"),
        created_at: r.get("created_at"),
        last_seen_at: r.get("last_seen_at"),
    }))
}

/// Set the app's default provider/model.
///
/// This replaces V1's global `fallback_priority` sort as the answer to "which
/// provider does this caller get when it asks for none". That sort was
/// daemon-wide, which is why Kitty had to PATCH `fallback_priority: 100` onto
/// every row it did not own in order to express "use mine"
/// (`src-tauri/src/bigtiny/providers.rs:222`) -- a write that would clobber
/// every other app's choice the moment a second one existed.
pub async fn set_app_default(
    pool: &SqlitePool,
    app_id: &str,
    provider_id: Option<&str>,
    model: Option<&str>,
) -> Result<u64, StorageError> {
    let result =
        sqlx::query("UPDATE apps SET default_provider_id = ?, default_model = ? WHERE id = ?")
            .bind(provider_id)
            .bind(model)
            .bind(app_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

/// Record liveness. Called on a coarse interval, never per request.
pub async fn touch_last_seen(pool: &SqlitePool, app_id: &str) -> Result<(), StorageError> {
    sqlx::query("UPDATE apps SET last_seen_at = datetime('now') WHERE id = ?")
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_apps(pool: &SqlitePool) -> Result<Vec<AppRow>, StorageError> {
    let rows = sqlx::query(
        "SELECT id, display_name, scopes, default_provider_id, default_model, \
                created_at, last_seen_at \
         FROM apps ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| AppRow {
            id: r.get("id"),
            display_name: r.get("display_name"),
            scopes: parse_scopes(r.get::<String, _>("scopes").as_str()),
            default_provider_id: r.get("default_provider_id"),
            default_model: r.get("default_model"),
            created_at: r.get("created_at"),
            last_seen_at: r.get("last_seen_at"),
        })
        .collect())
}

/// Revoke an app. Its rows are left in place rather than cascaded away.
///
/// Deleting an app's sessions along with its key would make a mistyped
/// revocation unrecoverable, and the rows remain perfectly valid -- they simply
/// become unreachable until the same `app_id` is registered again, which is a
/// deliberate and useful recovery path for an app that lost its stored key.
pub async fn delete_app(pool: &SqlitePool, app_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query("DELETE FROM apps WHERE id = ?")
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn hashing_is_stable_and_distinguishes_keys() {
        assert_eq!(hash_key("abc"), hash_key("abc"));
        assert_ne!(hash_key("abc"), hash_key("abd"));
        assert_eq!(hash_key("abc").len(), 64, "sha256 hex is 64 chars");
    }

    #[tokio::test]
    async fn a_registered_key_resolves_to_its_app() {
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "secret-key")
            .await
            .unwrap()
            .expect("first registration succeeds");

        let identity = identity_for_key(&pool, "secret-key").await.unwrap().unwrap();
        assert_eq!(identity.app_id, "kitty");
        assert!(identity.has_scope("anything"), "default scope is *");
    }

    #[tokio::test]
    async fn an_unknown_key_resolves_to_nothing() {
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "secret-key")
            .await
            .unwrap();
        assert!(identity_for_key(&pool, "wrong-key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_plaintext_key_is_never_stored() {
        // The property that makes a leaked database non-replayable.
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "secret-key")
            .await
            .unwrap();
        let stored: String = sqlx::query("SELECT key_hash FROM apps WHERE id = 'kitty'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("key_hash");
        assert_ne!(stored, "secret-key");
        assert_eq!(stored, hash_key("secret-key"));
    }

    #[tokio::test]
    async fn re_registering_an_existing_id_is_refused_without_replacing_the_key() {
        // An app that lost its key must not be able to take over the identity
        // by simply registering again -- that would be an auth bypass.
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "original").await.unwrap();

        let second = register_app(&pool, "kitty", "Impostor", "attacker-key")
            .await
            .unwrap();
        assert!(second.is_none(), "duplicate registration must be refused");

        assert!(identity_for_key(&pool, "attacker-key").await.unwrap().is_none());
        assert!(identity_for_key(&pool, "original").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_corrupt_scopes_blob_grants_nothing() {
        // Fail closed: unparseable JSON must not read as full access.
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "k").await.unwrap();
        sqlx::query("UPDATE apps SET scopes = 'not json' WHERE id = 'kitty'")
            .execute(&pool)
            .await
            .unwrap();

        let identity = identity_for_key(&pool, "k").await.unwrap().unwrap();
        assert!(!identity.has_scope("chat"));
        assert!(!identity.has_scope("*"));
    }

    #[tokio::test]
    async fn defaults_round_trip_and_can_be_cleared() {
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "k").await.unwrap();

        set_app_default(&pool, "kitty", Some("prov-1"), Some("gpt-x"))
            .await
            .unwrap();
        let app = get_app(&pool, "kitty").await.unwrap().unwrap();
        assert_eq!(app.default_provider_id.as_deref(), Some("prov-1"));
        assert_eq!(app.default_model.as_deref(), Some("gpt-x"));

        set_app_default(&pool, "kitty", None, None).await.unwrap();
        let app = get_app(&pool, "kitty").await.unwrap().unwrap();
        assert!(app.default_provider_id.is_none());
    }

    #[tokio::test]
    async fn two_apps_are_independent() {
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "key-a").await.unwrap();
        register_app(&pool, "notebook", "Notebook", "key-b")
            .await
            .unwrap();

        set_app_default(&pool, "kitty", Some("prov-a"), None).await.unwrap();
        set_app_default(&pool, "notebook", Some("prov-b"), None)
            .await
            .unwrap();

        // The V1 failure this replaces: one app choosing a provider used to
        // rewrite every other app's priority.
        assert_eq!(
            get_app(&pool, "kitty").await.unwrap().unwrap().default_provider_id,
            Some("prov-a".into())
        );
        assert_eq!(
            get_app(&pool, "notebook").await.unwrap().unwrap().default_provider_id,
            Some("prov-b".into())
        );
    }

    #[tokio::test]
    async fn deleting_an_app_revokes_its_key_but_keeps_its_rows() {
        let pool = pool().await;
        register_app(&pool, "kitty", "Kitty", "k").await.unwrap();
        sqlx::query("INSERT INTO sessions (id, name, app_id) VALUES ('s1', 'n', 'kitty')")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(delete_app(&pool, "kitty").await.unwrap(), 1);
        assert!(identity_for_key(&pool, "k").await.unwrap().is_none());

        // Rows survive, so a mistyped revocation is recoverable by
        // re-registering the same id.
        let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM sessions WHERE app_id = 'kitty'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
        assert_eq!(count, 1);
    }
}
