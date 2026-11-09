//! The self-tuning relevance floor, made deterministic. (Mirrors
//! adaptive-pathway's `recall::cut_log`; the two engines self-tune the same way.)
//!
//! Recall used to fold every turn's effective cut into a running EWMA as it
//! went, so a turn's floor depended on whatever any chat had recalled just
//! before it, and calling recall twice on the same message could give two
//! different blocks. Now a turn only *logs* its cut (`recall_cut_pending::<space>`
//! = `"<count>|<sum>"`), recall reads a snapshot (`recall_cut_ewma::<space>`)
//! that does not move during the day, and the maintenance heavy pass folds
//! the day's log into the snapshot ([`fold_pending`]). Same stored state and
//! same message, same block.

use crate::error::Result;
use crate::store::Db;

const EWMA_PREFIX: &str = "recall_cut_ewma::";
const PENDING_PREFIX: &str = "recall_cut_pending::";

/// The floor snapshot for `space`, or `None` before the first fold.
pub async fn snapshot(db: &Db, space: &str) -> Option<f64> {
    db.get_setting(&format!("{EWMA_PREFIX}{space}"))
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
}

/// Record one turn's effective cut, to be folded in tonight.
pub async fn log(db: &Db, space: &str, cut: f64) -> Result<()> {
    let key = format!("{PENDING_PREFIX}{space}");
    let (n, sum) = parse_pending(db.get_setting(&key).await?.as_deref());
    db.set_setting(&key, &format!("{}|{}", n + 1, sum + cut)).await
}

fn parse_pending(v: Option<&str>) -> (u64, f64) {
    v.and_then(|s| {
        let (n, sum) = s.split_once('|')?;
        Some((n.parse().ok()?, sum.parse().ok()?))
    })
    .unwrap_or((0, 0.0))
}

/// Fold every space's logged cuts into its snapshot, as if each had been
/// applied one at a time with smoothing `alpha` (with `n` samples of mean `m`:
/// `ewma' = (1-alpha)^n * ewma + (1 - (1-alpha)^n) * m`; a space with no
/// snapshot yet starts at `m`). Clears the log.
pub async fn fold_pending(db: &Db, alpha: f64) -> Result<()> {
    for (key, value) in db.settings_with_prefix(PENDING_PREFIX).await? {
        let space = &key[PENDING_PREFIX.len()..];
        let (n, sum) = parse_pending(Some(&value));
        if n > 0 {
            let mean = sum / n as f64;
            let next = match snapshot(db, space).await {
                Some(prev) => {
                    let keep = (1.0 - alpha).powi(n.min(i32::MAX as u64) as i32);
                    keep * prev + (1.0 - keep) * mean
                }
                None => mean,
            };
            db.set_setting(&format!("{EWMA_PREFIX}{space}"), &next.to_string())
                .await?;
        }
        db.delete_setting(&key).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logging_does_not_move_the_snapshot_until_folded() {
        let db = Db::open_in_memory().await.unwrap();
        assert_eq!(snapshot(&db, "s").await, None);
        log(&db, "s", 0.2).await.unwrap();
        log(&db, "s", 0.4).await.unwrap();
        assert_eq!(snapshot(&db, "s").await, None, "recall reads a stable snapshot");
        fold_pending(&db, 0.1).await.unwrap();
        assert!((snapshot(&db, "s").await.unwrap() - 0.3).abs() < 1e-12, "first fold starts at the mean");

        log(&db, "s", 0.5).await.unwrap();
        fold_pending(&db, 0.1).await.unwrap();
        let expect = 0.9 * 0.3 + 0.1 * 0.5;
        assert!((snapshot(&db, "s").await.unwrap() - expect).abs() < 1e-12);
        // The log is cleared: a second fold changes nothing.
        fold_pending(&db, 0.1).await.unwrap();
        assert!((snapshot(&db, "s").await.unwrap() - expect).abs() < 1e-12);
    }
}
