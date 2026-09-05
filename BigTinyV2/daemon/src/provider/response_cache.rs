//! Content-addressed response cache.
//!
//! A pipeline re-running over the same inputs re-pays for identical calls. This
//! is the obvious fix, and it has one non-obvious hazard worth stating plainly.
//!
//! # This is not `CacheConfig`
//!
//! The daemon already has a `cache` config, and it is about something else
//! entirely: prompt-*prefix* determinism, so a provider's KV cache hits across
//! turns (slot pinning, Anthropic `cache_control`). That makes the *same* call
//! cheaper. This makes a *repeat* call free. Keep the names apart.
//!
//! # Per-app by default
//!
//! **A shared cache is a tenancy leak.** Serving app Y a response derived from
//! app X's prompt hands one app the other's data, and it would be invisible —
//! no error, no log line, just a cache hit. So the key includes the app id, and
//! sharing is opt-in per request rather than the default.
//!
//! # What is never cached
//!
//! - **Turns that ran tools.** A tool call has side effects; replaying its
//!   answer without re-running it claims work that did not happen.
//! - **Errors and partial responses.** Caching a failure would make a transient
//!   outage permanent for the length of the TTL.
//!
//! # Why the key is a cryptographic hash
//!
//! A hit here does not report a hint or a statistic — it *returns a response*
//! in place of calling the model. So a colliding key does not degrade the
//! cache, it answers one prompt with another prompt's reply, and does so
//! looking exactly like success. The app id is inside the key rather than in a
//! `WHERE` clause precisely so a private and a shared entry cannot collide by
//! accident; that argument only holds if collisions are infeasible at all,
//! which a 64-bit `DefaultHasher` does not give (birthday bound ~2^32).
//! SHA-256 over a canonical encoding costs microseconds against a network
//! round trip, and `sha2` is already a dependency.
//!
//! # A hit skips the queue entirely
//!
//! No provider permit is acquired for a cache hit. That is the whole point —
//! a hit that queued behind live traffic would save the tokens but not the
//! latency, which is most of what a pipeline is buying.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::error::StorageError;

/// What a caller asked for, per request.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CacheDirective {
    /// Consult the cache. Default false for `/send` (chat nondeterminism is
    /// usually wanted) and true for jobs, which is the case that benefits.
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    /// Seconds. `None` means the daemon default.
    #[serde(default)]
    pub ttl_s: Option<i64>,
    /// Opt in to the pool shared across apps.
    ///
    /// Off by default, and deliberately awkward to reach: the failure mode is
    /// silent cross-app data exposure, so it should be a decision rather than
    /// something inherited.
    #[serde(default)]
    pub shared: bool,
}

impl Default for CacheDirective {
    fn default() -> Self {
        Self {
            read: false,
            write: false,
            ttl_s: None,
            shared: false,
        }
    }
}

impl CacheDirective {
    /// The default for a detached job: caching on, private to the app.
    pub fn for_job() -> Self {
        Self {
            read: true,
            write: true,
            ttl_s: None,
            shared: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.read || self.write
    }
}

/// Default lifetime of an entry.
pub const DEFAULT_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Everything that can change a response, hashed into one key.
///
/// Missing any of these would serve a response generated under different
/// conditions — a different model, different sampling, a different tool set —
/// which is worse than a miss, because it looks like success.
#[allow(clippy::too_many_arguments)]
pub fn cache_key(
    app_id: &str,
    shared: bool,
    provider_id: &str,
    model: &str,
    messages: &[Value],
    tools: Option<&[Value]>,
    sampling: &Value,
    response_schema: Option<&Value>,
) -> String {
    let mut hasher = Sha256::new();

    // Every field is length-prefixed and separated. Without that, two
    // different inputs can serialize to the same byte stream -- a provider
    // named "a" with model "bc" hashes identically to "ab" with "c" -- and
    // that is a collision an ordinary configuration change could produce, not
    // an adversarial one.
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };

    // The tenancy boundary, in the key itself rather than in a WHERE clause:
    // a private entry and a shared one cannot collide even by accident.
    field(if shared { b"__shared__" } else { app_id.as_bytes() });
    field(provider_id.as_bytes());
    field(model.as_bytes());

    // `serde_json` cannot fail on a `Value` -- it holds no non-finite floats,
    // no non-string map keys, no cycles. `expect` rather than
    // `unwrap_or_default`, because the old fallback silently hashed the empty
    // string, which would have collapsed every failure onto one key and
    // served whatever was cached there.
    let json = |v: &Value| serde_json::to_vec(v).expect("a Value always serializes");
    field(&json(&Value::Array(messages.to_vec())));
    field(&tools.map(|t| json(&Value::Array(t.to_vec()))).unwrap_or_default());
    field(&json(sampling));
    field(&response_schema.map(json).unwrap_or_default());

    format!("{:x}", hasher.finalize())
}

/// Drop expired rows.
///
/// `get` already filters on `expires_at`, so a stale row is never *served* --
/// but nothing deleted it either, and the table only grows. Run from the
/// daily retention sweep rather than on its own timer: it is the same kind of
/// work, wants the same "not during a turn" scheduling, and one sweep is
/// easier to reason about than two.
pub async fn prune_expired(pool: &SqlitePool) -> Result<u64, StorageError> {
    let out = sqlx::query("DELETE FROM response_cache WHERE expires_at <= datetime('now')")
        .execute(pool)
        .await?;
    Ok(out.rows_affected())
}

/// Look up a cached response, if it has not expired.
pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<String>, StorageError> {
    let hit: Option<String> = sqlx::query_scalar(
        "SELECT response FROM response_cache \
         WHERE key = ? AND expires_at > datetime('now')",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(hit)
}

/// Store a response.
pub async fn put(
    pool: &SqlitePool,
    key: &str,
    response: &str,
    ttl_s: i64,
) -> Result<(), StorageError> {
    let ttl = ttl_s.clamp(1, 365 * 24 * 60 * 60);
    sqlx::query(
        "INSERT INTO response_cache (key, response, expires_at) \
         VALUES (?1, ?2, datetime('now', '+' || ?3 || ' seconds')) \
         ON CONFLICT(key) DO UPDATE SET response = excluded.response, \
         expires_at = excluded.expires_at",
    )
    .bind(key)
    .bind(response)
    .bind(ttl)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop expired entries. Called on the retention sweep.
pub async fn sweep(pool: &SqlitePool) -> Result<u64, StorageError> {
    let result = sqlx::query("DELETE FROM response_cache WHERE expires_at <= datetime('now')")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn key_for(app: &str, shared: bool, model: &str) -> String {
        cache_key(
            app,
            shared,
            "prov",
            model,
            &[json!({"role": "user", "content": "hi"})],
            None,
            &json!({"temperature": 0.0}),
            None,
        )
    }

    #[test]
    fn two_apps_get_different_keys_for_identical_requests() {
        // The tenancy property. Without it, app B's identical prompt would be
        // served app A's response -- silently, with no error and no log line.
        assert_ne!(key_for("app-a", false, "m"), key_for("app-b", false, "m"));
    }

    #[test]
    fn the_shared_pool_collapses_apps_onto_one_key() {
        // Only reachable by explicit opt-in, which is the point.
        assert_eq!(key_for("app-a", true, "m"), key_for("app-b", true, "m"));
    }

    #[test]
    fn a_private_entry_can_never_collide_with_a_shared_one() {
        assert_ne!(key_for("app-a", false, "m"), key_for("app-a", true, "m"));
    }

    #[test]
    fn everything_that_changes_a_response_changes_the_key() {
        let base = key_for("a", false, "model-1");
        assert_ne!(base, key_for("a", false, "model-2"), "model");

        let msgs = |c: &str| vec![json!({"role": "user", "content": c})];
        let with = |m: Vec<Value>, s: Value, sc: Option<Value>| {
            cache_key("a", false, "prov", "m", &m, None, &s, sc.as_ref())
        };
        let b = with(msgs("hi"), json!({"temperature": 0.0}), None);
        assert_ne!(b, with(msgs("bye"), json!({"temperature": 0.0}), None), "messages");
        assert_ne!(b, with(msgs("hi"), json!({"temperature": 0.9}), None), "sampling");
        assert_ne!(
            b,
            with(msgs("hi"), json!({"temperature": 0.0}), Some(json!({"type": "object"}))),
            "response schema"
        );

        let tools = vec![json!({"name": "read_file"})];
        assert_ne!(
            b,
            cache_key("a", false, "prov", "m", &msgs("hi"), Some(&tools), &json!({"temperature": 0.0}), None),
            "tool set"
        );
    }

    #[test]
    fn the_same_request_hashes_stably() {
        assert_eq!(key_for("a", false, "m"), key_for("a", false, "m"));
    }

    #[tokio::test]
    async fn a_stored_response_round_trips() {
        let pool = pool().await;
        put(&pool, "k1", "the answer", 60).await.unwrap();
        assert_eq!(get(&pool, "k1").await.unwrap().as_deref(), Some("the answer"));
        assert_eq!(get(&pool, "never-written").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_expired_entry_is_a_miss_and_is_swept() {
        let pool = pool().await;
        // Write directly with an expiry in the past: `put` clamps the TTL to
        // at least a second, which is right for the API and unhelpful here.
        sqlx::query(
            "INSERT INTO response_cache (key, response, expires_at) \
             VALUES ('old', 'stale', datetime('now', '-10 seconds'))",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(get(&pool, "old").await.unwrap(), None, "expired is a miss");
        assert_eq!(sweep(&pool).await.unwrap(), 1);
        assert_eq!(sweep(&pool).await.unwrap(), 0, "sweep is idempotent");
    }

    #[tokio::test]
    async fn writing_the_same_key_twice_replaces_rather_than_erroring() {
        let pool = pool().await;
        put(&pool, "k", "first", 60).await.unwrap();
        put(&pool, "k", "second", 60).await.unwrap();
        assert_eq!(get(&pool, "k").await.unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn a_job_caches_privately_by_default() {
        let d = CacheDirective::for_job();
        assert!(d.read && d.write);
        assert!(!d.shared, "sharing must be a decision, never inherited");
    }

    #[test]
    fn the_default_directive_is_off() {
        // Chat nondeterminism is usually wanted; caching it would surprise.
        assert!(!CacheDirective::default().is_active());
    }

    #[test]
    fn adjacent_fields_cannot_be_confused_for_one_another() {
        // Without length prefixes, concatenation makes ("a","bc") and
        // ("ab","c") the same byte stream -- a collision an ordinary rename
        // could produce, not an adversarial one. And a collision here does not
        // degrade the cache, it answers one prompt with another's reply.
        let key = |provider: &str, model: &str| {
            cache_key(
                "app-a",
                false,
                provider,
                model,
                &[json!({"role": "user", "content": "hi"})],
                None,
                &json!({}),
                None,
            )
        };
        assert_ne!(key("a", "bc"), key("ab", "c"));
    }

    #[test]
    fn an_app_cannot_collide_with_the_shared_pool_or_with_another_app() {
        // The tenancy boundary is the key itself, so this is the assertion
        // that the boundary exists at all.
        let key = |app: &str, shared: bool| {
            cache_key(
                app,
                shared,
                "p",
                "m",
                &[json!({"role": "user", "content": "hi"})],
                None,
                &json!({}),
                None,
            )
        };
        assert_ne!(key("app-a", false), key("app-b", false));
        assert_ne!(key("app-a", false), key("app-a", true));
        assert_eq!(
            key("app-a", true),
            key("app-b", true),
            "the shared pool is the same entry for everyone, by construction"
        );
    }

    #[test]
    fn the_key_is_a_full_sha256_and_is_stable() {
        let key = cache_key(
            "app-a",
            false,
            "p",
            "m",
            &[json!({"role": "user", "content": "hi"})],
            None,
            &json!({}),
            None,
        );
        assert_eq!(key.len(), 64, "a truncated key is a weaker key");
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
