//! Reading and writing one run's ledger: what the tool server, the driver and
//! the routes share. Errors are plain strings written for whoever reads them
//! next (a model, or a route's JSON).

use serde_json::Value;
use sqlx::SqlitePool;

use crate::research::contract::Contract;
use crate::research::ledger::{Body, Finding, Ledger, Source};
use crate::storage::research::{self as store, Appended, RunRow};
use crate::storage::sessions;

/// Session metadata keys that tie a session to a run.
pub const META_RUN: &str = "research_run_id";
pub const META_ROLE: &str = "research_role";
pub const META_WAVE: &str = "research_wave";

/// Turn 0, on the run's own chat: drafts and submits the contract.
pub const ROLE_PLAN: &str = "plan";
/// A wave's planner, on a fresh child session: merges, writes leads, sets the
/// frontier.
pub const ROLE_PLANNER: &str = "planner";

/// A run with its parsed contract and ledger.
pub struct Loaded {
    pub run: RunRow,
    pub contract: Option<Contract>,
    pub ledger: Ledger,
}

pub async fn load(pool: &SqlitePool, run_id: &str) -> Result<Loaded, String> {
    let run = store::get_run(pool, run_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("research run {run_id} not found"))?;
    let contract = run
        .contract
        .as_deref()
        .and_then(|c| serde_json::from_str::<Contract>(c).ok());
    let ledger = load_ledger(pool, run_id).await?;
    Ok(Loaded {
        run,
        contract,
        ledger,
    })
}

pub async fn load_ledger(pool: &SqlitePool, run_id: &str) -> Result<Ledger, String> {
    let (e, l, t) = tokio::try_join!(
        store::entries(pool, run_id),
        store::links(pool, run_id),
        store::tags(pool, run_id),
    )
    .map_err(|e| e.to_string())?;
    Ok(Ledger::build(e, l, t))
}

pub async fn append(
    pool: &SqlitePool,
    run_id: &str,
    wave: i64,
    body: &Body,
) -> Result<Appended, String> {
    store::append_entry(
        pool,
        run_id,
        wave,
        body.kind(),
        &body.payload_json(),
        body.norm_key().as_deref(),
    )
    .await
    .map_err(|e| e.to_string())
}

pub async fn link(
    pool: &SqlitePool,
    run_id: &str,
    from: i64,
    to: i64,
    kind: &str,
    reason: &str,
    wave: i64,
) -> Result<bool, String> {
    store::add_link(pool, run_id, from, to, kind, reason, wave)
        .await
        .map_err(|e| e.to_string())
}

/// Place an entry against the contract's cells and sections, ignoring ids
/// the contract does not have. Returns the ids that were unknown.
pub async fn place(
    pool: &SqlitePool,
    run_id: &str,
    contract: &Contract,
    seq: i64,
    cells: &[String],
    sections: &[String],
    wave: i64,
) -> Result<Vec<String>, String> {
    let mut unknown = Vec::new();
    for c in cells {
        if contract.has_cell(c) {
            store::add_tag(pool, run_id, seq, "cell", c, wave)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            unknown.push(c.clone());
        }
    }
    for s in sections {
        if contract.has_section(s) {
            store::add_tag(pool, run_id, seq, "section", s, wave)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            unknown.push(s.clone());
        }
    }
    Ok(unknown)
}

/// What recording one finding did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub finding: Appended,
    pub source: Option<i64>,
}

/// Record a finding with its source: the source entry (once per page), the
/// finding (once per claim), the `cites` link, and its placement. An exact
/// repeat of a claim from a *different* page still gains that page as a
/// source, which is what corroboration counts.
#[allow(clippy::too_many_arguments)]
pub async fn record_finding(
    pool: &SqlitePool,
    run_id: &str,
    contract: Option<&Contract>,
    wave: i64,
    finding: Finding,
    source_title: &str,
    cells: &[String],
    sections: &[String],
) -> Result<Recorded, String> {
    let source = match finding.source_url.as_deref() {
        Some(url) if !url.trim().is_empty() => Some(
            append(
                pool,
                run_id,
                wave,
                &Body::Source(Source {
                    url: url.to_string(),
                    title: source_title.to_string(),
                    date: String::new(),
                    cached_path: None,
                    document_id: None,
                }),
            )
            .await?
            .seq(),
        ),
        _ => None,
    };
    let appended = append(pool, run_id, wave, &Body::Finding(finding)).await?;
    if let Some(src) = source {
        link(pool, run_id, appended.seq(), src, "cites", "", wave).await?;
    }
    if let Some(contract) = contract {
        place(
            pool,
            run_id,
            contract,
            appended.seq(),
            cells,
            sections,
            wave,
        )
        .await?;
    }
    Ok(Recorded {
        finding: appended,
        source,
    })
}

/// The research role a session plays, from its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRole {
    pub run_id: String,
    pub role: String,
    pub wave: i64,
}

pub fn role_from_metadata(metadata: &Value) -> Option<SessionRole> {
    let run_id = metadata.get(META_RUN)?.as_str()?.to_string();
    let role = metadata.get(META_ROLE)?.as_str()?.to_string();
    let wave = metadata.get(META_WAVE).and_then(Value::as_i64).unwrap_or(0);
    Some(SessionRole { run_id, role, wave })
}

pub async fn session_role(pool: &SqlitePool, session_id: &str) -> Option<SessionRole> {
    let meta = sessions::get_session_metadata(pool, session_id)
        .await
        .ok()
        .flatten()?;
    let meta: Value = serde_json::from_str(&meta).ok()?;
    role_from_metadata(&meta)
}
