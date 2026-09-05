//! Durable job rows. See `migrations/019_jobs.sql` for the state machine.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct JobRow {
    pub id: String,
    pub app_id: String,
    pub session_id: Option<String>,
    pub prompt: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub options: Option<String>,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

const COLS: &str = "id, app_id, session_id, prompt, status, result, error, options, \
                    created_at, started_at, finished_at";

pub async fn create(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
    session_id: &str,
    prompt: &str,
    options: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO jobs (id, app_id, session_id, prompt, options) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(app_id)
    .bind(session_id)
    .bind(prompt)
    .bind(options)
    .execute(pool)
    .await?;
    Ok(())
}

/// One job, if this app owns it.
pub async fn get_for_app(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
) -> Result<Option<JobRow>, StorageError> {
    let sql = format!("SELECT {COLS} FROM jobs WHERE id = ? AND app_id = ?");
    Ok(sqlx::query_as::<_, JobRow>(&sql)
        .bind(id)
        .bind(app_id)
        .fetch_optional(pool)
        .await?)
}

/// This app's jobs, newest first, optionally filtered by status.
pub async fn list_for_app(
    pool: &SqlitePool,
    app_id: &str,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<JobRow>, StorageError> {
    // Clamped by the caller, but bounded here too: SQLite treats a negative
    // LIMIT as "no limit", so an unclamped value reads the whole table.
    let limit = limit.clamp(1, 500);
    let sql = format!(
        "SELECT {COLS} FROM jobs WHERE app_id = ?1 AND (?2 IS NULL OR status = ?2) \
         ORDER BY created_at DESC LIMIT ?3"
    );
    Ok(sqlx::query_as::<_, JobRow>(&sql)
        .bind(app_id)
        .bind(status)
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

/// Jobs in a given status across *every* app.
///
/// Deliberately unscoped: the idle-exit timer asks "is any work in flight on
/// this daemon", which is a daemon-wide question. Not reachable from a route.
pub async fn list_by_status(
    pool: &SqlitePool,
    status: &str,
    limit: i64,
) -> Result<Vec<JobRow>, StorageError> {
    let sql = format!("SELECT {COLS} FROM jobs WHERE status = ? LIMIT ?");
    Ok(sqlx::query_as::<_, JobRow>(&sql)
        .bind(status)
        .bind(limit.clamp(1, 500))
        .fetch_all(pool)
        .await?)
}

pub async fn mark_running(pool: &SqlitePool, id: &str) -> Result<(), StorageError> {
    sqlx::query("UPDATE jobs SET status = 'running', started_at = datetime('now') WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Move a job that has not already finished to a terminal state.
///
/// Conditional, because the turn task and a `DELETE /api/jobs/{id}` race by
/// construction: the cancel marks the row `cancelled` while the turn is still
/// unwinding, and an unconditional write then relabelled it `succeeded`. The
/// owner cancelled the job and was told it had completed.
///
/// The guard is "not already terminal" rather than "is running": `mark_running`
/// can itself fail, and a turn that dies before it lands must still be able to
/// record why. Only a state someone has already been told about is protected.
///
/// Returns rows affected, so a caller can tell a real transition from a no-op.
pub async fn finish(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<u64, StorageError> {
    let out = sqlx::query(
        "UPDATE jobs SET status = ?, result = ?, error = ?, finished_at = datetime('now') \
         WHERE id = ? AND status IN ('pending', 'running')",
    )
    .bind(status)
    .bind(result)
    .bind(error)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(out.rows_affected())
}

/// Cancel a job this app owns, if it has not already finished.
///
/// Returns rows affected, so a caller can tell "cancelled" from "it had
/// already finished" rather than reporting a misleading success.
pub async fn cancel_for_app(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
) -> Result<u64, StorageError> {
    let result = sqlx::query(
        "UPDATE jobs SET status = 'cancelled', finished_at = datetime('now') \
         WHERE id = ? AND app_id = ? AND status IN ('pending','running')",
    )
    .bind(id)
    .bind(app_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Mark everything left `running` by a previous process as `interrupted`.
///
/// Called once at startup. **Deliberately not a re-queue**: a turn may have
/// executed tools with side effects -- files written, requests sent -- and
/// silently re-running it would repeat them. The owner sees `interrupted` and
/// decides whether resubmitting is safe.
pub async fn mark_interrupted_on_boot(pool: &SqlitePool) -> Result<u64, StorageError> {
    let result = sqlx::query(
        "UPDATE jobs SET status = 'interrupted', finished_at = datetime('now'), \
         error = 'the daemon stopped while this job was running' \
         WHERE status IN ('pending','running')",
    )
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
        for app in ["app-a", "app-b"] {
            crate::storage::apps::register_app(&pool, app, app, &format!("k-{app}"))
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO sessions (id, name, status, app_id) VALUES ('s1','n','active','app-a')")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn a_job_is_invisible_to_another_app() {
        let pool = pool().await;
        create(&pool, "j1", "app-a", "s1", "do it", None).await.unwrap();

        assert!(get_for_app(&pool, "j1", "app-a").await.unwrap().is_some());
        assert!(get_for_app(&pool, "j1", "app-b").await.unwrap().is_none());
        assert!(list_for_app(&pool, "app-b", None, 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_job_runs_through_its_lifecycle() {
        let pool = pool().await;
        create(&pool, "j1", "app-a", "s1", "do it", None).await.unwrap();
        assert_eq!(get_for_app(&pool, "j1", "app-a").await.unwrap().unwrap().status, "pending");

        mark_running(&pool, "j1").await.unwrap();
        let running = get_for_app(&pool, "j1", "app-a").await.unwrap().unwrap();
        assert_eq!(running.status, "running");
        assert!(running.started_at.is_some());

        finish(&pool, "j1", "succeeded", Some("answer"), None).await.unwrap();
        let done = get_for_app(&pool, "j1", "app-a").await.unwrap().unwrap();
        assert_eq!(done.status, "succeeded");
        assert_eq!(done.result.as_deref(), Some("answer"));
        assert!(done.finished_at.is_some());
    }

    #[tokio::test]
    async fn cancelling_a_finished_job_reports_no_change() {
        // Reporting success would tell a caller it had stopped work that had
        // in fact already completed.
        let pool = pool().await;
        create(&pool, "j1", "app-a", "s1", "x", None).await.unwrap();
        finish(&pool, "j1", "succeeded", Some("done"), None).await.unwrap();

        assert_eq!(cancel_for_app(&pool, "j1", "app-a").await.unwrap(), 0);
        assert_eq!(
            get_for_app(&pool, "j1", "app-a").await.unwrap().unwrap().status,
            "succeeded"
        );
    }

    #[tokio::test]
    async fn another_app_cannot_cancel_my_job() {
        let pool = pool().await;
        create(&pool, "j1", "app-a", "s1", "x", None).await.unwrap();
        assert_eq!(cancel_for_app(&pool, "j1", "app-b").await.unwrap(), 0);
        assert_eq!(
            get_for_app(&pool, "j1", "app-a").await.unwrap().unwrap().status,
            "pending"
        );
    }

    #[tokio::test]
    async fn a_crash_leaves_jobs_interrupted_rather_than_requeued() {
        // The property that stops a job with side effects from silently
        // re-running: `interrupted` is terminal, and resubmitting is the
        // owner's decision.
        let pool = pool().await;
        create(&pool, "j1", "app-a", "s1", "x", None).await.unwrap();
        create(&pool, "j2", "app-a", "s1", "y", None).await.unwrap();
        mark_running(&pool, "j2").await.unwrap();
        create(&pool, "j3", "app-a", "s1", "z", None).await.unwrap();
        finish(&pool, "j3", "succeeded", Some("r"), None).await.unwrap();

        assert_eq!(mark_interrupted_on_boot(&pool).await.unwrap(), 2);

        for (id, expected) in [("j1", "interrupted"), ("j2", "interrupted"), ("j3", "succeeded")] {
            assert_eq!(
                get_for_app(&pool, id, "app-a").await.unwrap().unwrap().status,
                expected,
                "job {id}"
            );
        }
    }

    #[tokio::test]
    async fn listing_filters_by_status_and_bounds_the_page() {
        let pool = pool().await;
        for i in 0..3 {
            create(&pool, &format!("j{i}"), "app-a", "s1", "x", None).await.unwrap();
        }
        finish(&pool, "j0", "failed", None, Some("boom")).await.unwrap();

        assert_eq!(list_for_app(&pool, "app-a", None, 50).await.unwrap().len(), 3);
        assert_eq!(
            list_for_app(&pool, "app-a", Some("failed"), 50).await.unwrap().len(),
            1
        );
        // A negative limit must not read the whole table.
        assert!(!list_for_app(&pool, "app-a", None, -1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn finishing_a_cancelled_job_does_not_resurrect_it_as_succeeded() {
        // The detached turn task and `DELETE /api/jobs/{id}` race by
        // construction: cancel lands while the turn is still unwinding, and
        // the turn's own `finish` used to overwrite it. The owner cancelled
        // the job and was then told it had completed.
        let pool = pool().await;
        create(&pool, "j1", "app-a", "s1", "prompt", None)
            .await
            .unwrap();
        mark_running(&pool, "j1").await.unwrap();

        assert_eq!(cancel_for_app(&pool, "j1", "app-a").await.unwrap(), 1);
        let changed = finish(&pool, "j1", "succeeded", Some("done"), None)
            .await
            .unwrap();

        assert_eq!(changed, 0, "finish overwrote a terminal state");
        let job = get_for_app(&pool, "j1", "app-a").await.unwrap().unwrap();
        assert_eq!(job.status, "cancelled");
        assert!(job.result.is_none(), "a cancelled job reported a result");
    }

    #[tokio::test]
    async fn a_turn_that_dies_before_mark_running_can_still_record_why() {
        // The guard is "not already terminal", not "is running" --
        // `mark_running` can itself fail, and a job that never got there must
        // still be able to report its own failure rather than sitting
        // `pending` forever.
        let pool = pool().await;
        create(&pool, "j2", "app-a", "s1", "prompt", None)
            .await
            .unwrap();

        let changed = finish(&pool, "j2", "failed", None, Some("no provider"))
            .await
            .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(
            get_for_app(&pool, "j2", "app-a").await.unwrap().unwrap().status,
            "failed"
        );
    }
}
