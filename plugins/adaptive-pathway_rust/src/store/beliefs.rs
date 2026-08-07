//! `beliefs` table access. A belief is the atomic unit of behavioral memory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{decode_embedding, encode_embedding, Db};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Identity,
    Context,
    Conversation,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Identity => "identity",
            Layer::Context => "context",
            Layer::Conversation => "conversation",
        }
    }

    /// The extractor can never write identity -- only consolidation promotes
    /// (this is a schema-level guard in 001_init.sql CHECK, mirrored here so
    /// the enum can be used before any SQL is written).
    pub fn extractor_writable(self) -> bool {
        matches!(self, Layer::Context | Layer::Conversation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Correction,
    DirectStatement,
    ControlledTest,
    InferredPattern,
    SingleObservation,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Correction => "correction",
            Provenance::DirectStatement => "direct_statement",
            Provenance::ControlledTest => "controlled_test",
            Provenance::InferredPattern => "inferred_pattern",
            Provenance::SingleObservation => "single_observation",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "correction" => Provenance::Correction,
            "direct_statement" => Provenance::DirectStatement,
            "controlled_test" => Provenance::ControlledTest,
            "inferred_pattern" => Provenance::InferredPattern,
            _ => Provenance::SingleObservation,
        }
    }

    /// Initial confidence a new belief starts at for this provenance.
    pub fn initial_confidence(self) -> f64 {
        match self {
            Provenance::Correction => 0.75,
            Provenance::DirectStatement => 0.70,
            Provenance::ControlledTest => 0.65,
            Provenance::InferredPattern => 0.30,
            Provenance::SingleObservation => 0.15,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Belief {
    pub id: String,
    pub text: String,
    pub embedding: Vec<f32>,
    pub confidence: f64,
    pub provenance: Provenance,
    pub layer: Layer,
    pub tested: bool,
    pub domain: Option<String>,
    pub tier: String,
    pub support_count: i64,
    pub distinct_sessions: i64,
    pub contradict_count: i64,
    pub pinned: bool,
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub consolidated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct BeliefPatch {
    pub confidence: Option<f64>,
    pub tested: Option<bool>,
    pub support_count: Option<i64>,
    pub distinct_sessions: Option<i64>,
    pub contradict_count: Option<i64>,
    pub pinned: Option<bool>,
    pub domain: Option<Option<String>>,
    pub layer: Option<Layer>,
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub consolidated_at: Option<DateTime<Utc>>,
}

impl Db {
    pub async fn insert_belief(&self, b: &Belief) -> Result<()> {
        sqlx::query(
            "INSERT INTO beliefs (id, text, embedding, confidence, provenance, layer, tested, \
             domain, tier, support_count, distinct_sessions, contradict_count, pinned, \
             last_confirmed_at, consolidated_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&b.id)
        .bind(&b.text)
        .bind(encode_embedding(&b.embedding))
        .bind(b.confidence)
        .bind(b.provenance.as_str())
        .bind(b.layer.as_str())
        .bind(b.tested)
        .bind(&b.domain)
        .bind(&b.tier)
        .bind(b.support_count)
        .bind(b.distinct_sessions)
        .bind(b.contradict_count)
        .bind(b.pinned)
        .bind(b.last_confirmed_at)
        .bind(b.consolidated_at)
        .bind(b.created_at)
        .bind(b.updated_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn update_belief(&self, id: &str, p: &BeliefPatch, updated_at: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE beliefs SET confidence = COALESCE(?, confidence), \
                     tested = COALESCE(?, tested), \
                     support_count = COALESCE(?, support_count), \
                     distinct_sessions = COALESCE(?, distinct_sessions), \
                     contradict_count = COALESCE(?, contradict_count), \
                     pinned = COALESCE(?, pinned), \
                     domain = COALESCE(?, domain), \
                     layer = COALESCE(?, layer), \
                     last_confirmed_at = COALESCE(?, last_confirmed_at), \
                     consolidated_at = COALESCE(?, consolidated_at), \
                     updated_at = ? WHERE id = ?")
            .bind(p.confidence)
            .bind(p.tested)
            .bind(p.support_count)
            .bind(p.distinct_sessions)
            .bind(p.contradict_count)
            .bind(p.pinned)
            .bind(p.domain.clone().flatten())
            .bind(p.layer.map(|l| l.as_str()))
            .bind(p.last_confirmed_at)
            .bind(p.consolidated_at)
            .bind(updated_at)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn get_belief(&self, id: &str) -> Result<Option<Belief>> {
        let row = sqlx::query_as::<_, BeliefRow>("SELECT * FROM beliefs WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(map_belief))
    }

    pub async fn list_beliefs(&self, layer: Option<Layer>) -> Result<Vec<Belief>> {
        let rows = match layer {
            Some(l) => {
                sqlx::query_as::<_, BeliefRow>("SELECT * FROM beliefs WHERE layer = ?")
                    .bind(l.as_str())
                    .fetch_all(self.pool())
                    .await?
            }
            None => {
                sqlx::query_as::<_, BeliefRow>("SELECT * FROM beliefs")
                    .fetch_all(self.pool())
                    .await?
            }
        };
        Ok(rows.into_iter().map(map_belief).collect())
    }

    pub async fn delete_belief(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM beliefs WHERE id = ?").bind(id).execute(self.pool()).await?;
        Ok(())
    }

    /// All beliefs with embeddings loaded, for in-memory vector search.
    pub async fn load_embeddings(&self, layer: Option<Layer>) -> Result<Vec<Belief>> {
        self.list_beliefs(layer).await
    }
}

struct BeliefRow {
    id: String,
    text: String,
    embedding: Vec<u8>,
    confidence: f64,
    provenance: String,
    layer: String,
    tested: bool,
    domain: Option<String>,
    tier: String,
    support_count: i64,
    distinct_sessions: i64,
    contradict_count: i64,
    pinned: bool,
    last_confirmed_at: Option<DateTime<Utc>>,
    consolidated_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl sqlx::FromRow<'_, sqlx::sqlite::SqliteRow> for BeliefRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            text: row.try_get("text")?,
            embedding: row.try_get("embedding")?,
            confidence: row.try_get("confidence")?,
            provenance: row.try_get("provenance")?,
            layer: row.try_get("layer")?,
            tested: row.try_get("tested")?,
            domain: row.try_get("domain")?,
            tier: row.try_get("tier")?,
            support_count: row.try_get("support_count")?,
            distinct_sessions: row.try_get("distinct_sessions")?,
            contradict_count: row.try_get("contradict_count")?,
            pinned: row.try_get("pinned")?,
            last_confirmed_at: row.try_get("last_confirmed_at")?,
            consolidated_at: row.try_get("consolidated_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

fn map_belief(r: BeliefRow) -> Belief {
    Belief {
        id: r.id,
        text: r.text,
        embedding: decode_embedding(&r.embedding),
        confidence: r.confidence,
        provenance: Provenance::parse(&r.provenance),
        layer: if r.layer == "identity" {
            Layer::Identity
        } else if r.layer == "context" {
            Layer::Context
        } else {
            Layer::Conversation
        },
        tested: r.tested,
        domain: r.domain,
        tier: r.tier,
        support_count: r.support_count,
        distinct_sessions: r.distinct_sessions,
        contradict_count: r.contradict_count,
        pinned: r.pinned,
        last_confirmed_at: r.last_confirmed_at,
        consolidated_at: r.consolidated_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}
