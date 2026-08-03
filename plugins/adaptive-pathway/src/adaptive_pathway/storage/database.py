import sqlalchemy as sa
from sqlalchemy.ext.asyncio import create_async_engine
from sqlalchemy.orm import declarative_base
from sqlalchemy import Column, String, Float, Integer, Boolean, DateTime, JSON, BLOB

Base = declarative_base()

class NodeModel(Base):
    __tablename__ = "nodes"
    id = Column(String, primary_key=True)
    context_embedding = Column(BLOB, nullable=True)
    features_json = Column(JSON, default=dict)
    status = Column(String, default="provisional")
    created_at = Column(DateTime, server_default=sa.func.now())

class EdgeModel(Base):
    __tablename__ = "edges"
    id = Column(String, primary_key=True)
    source_node_id = Column(String, nullable=True)
    target_node_id = Column(String, nullable=True)
    semantic_primitive = Column(String, index=True)
    confidence = Column(Float, default=0.5)
    last_accessed = Column(DateTime, server_default=sa.func.now())
    ttl = Column(DateTime, nullable=True)
    tags = Column(JSON, default=list)
    domain_tags = Column(JSON, default=list)
    domain_id = Column(String, nullable=True, index=True)
    tier = Column(String, default="hot")
    status = Column(String, default="provisional")
    primitive_source = Column(String, default="auto_named")
    auto_review_flagged = Column(Boolean, default=False)
    observed_reward = Column(Float, nullable=True)
    rationale_snapshot = Column(JSON, default=list)
    frequency = Column(Integer, default=0)
    created_at = Column(DateTime, server_default=sa.func.now())
    co_selected_with = Column(JSON, default=list)
    override_rate = Column(Float, default=0.0)

class DomainModel(Base):
    __tablename__ = "domains"
    id = Column(String, primary_key=True)
    name = Column(String)
    topic_id = Column(Integer, nullable=True)
    dpp_diversity_weight = Column(Float, default=1.0)
    novelty_lambda = Column(Float, default=0.5)
    domain_source = Column(String, default="auto_named")
    auto_review_flagged = Column(Boolean, default=False)
    revision_rate = Column(Float, default=0.0)
    acceptance_rate = Column(Float, default=0.0)
    sessions = Column(Integer, default=0)
    edge_count = Column(Integer, default=0)
    override_rate = Column(Float, default=0.0)
    last_inferred = Column(DateTime, nullable=True)
    user_override = Column(JSON, nullable=True)
    locked = Column(Boolean, default=False)
    # Normalized float32 centroid embedding used by DomainDiscovery.infer_domain
    # (added via ALTER for databases created before this column existed).
    centroid = Column(BLOB, nullable=True)

class AnnotationModel(Base):
    __tablename__ = "annotations"
    id = Column(String, primary_key=True)
    edge_id = Column(String, nullable=True, index=True)
    annotation_type = Column(String, index=True)
    intensity = Column(Float, default=0.5)
    detection_confidence = Column(Float, default=0.5)
    detection_method = Column(String, default="heuristic")
    behavioral_confirmation = Column(Boolean, default=False)
    multi_turn_resolved = Column(Boolean, default=False)
    timestamp = Column(DateTime, server_default=sa.func.now())
    session_id = Column(String, nullable=True)
    action_id = Column(String, nullable=True)
    reward_weight = Column(Float, nullable=True)
    context_snapshot = Column(JSON, nullable=True)

class OverrideLogModel(Base):
    __tablename__ = "override_log"
    id = Column(String, primary_key=True)
    task_id = Column(String, nullable=True)
    action_taken = Column(String)
    user_intent = Column(String)
    root_cause = Column(String, nullable=True)
    reward_signal = Column(Float, default=0.0)
    timestamp = Column(DateTime, server_default=sa.func.now())

class PassiveTelemetryModel(Base):
    __tablename__ = "passive_telemetry"
    id = Column(String, primary_key=True)
    edge_id = Column(String, index=True)
    signal_type = Column(String, index=True)
    weight = Column(Float)
    timestamp = Column(DateTime, server_default=sa.func.now())
    session_id = Column(String, index=True)

class FeedbackCentroidModel(Base):
    __tablename__ = "feedback_centroids"
    id = Column(String, primary_key=True)
    positive_centroid = Column(BLOB, nullable=True)
    negative_centroid = Column(BLOB, nullable=True)
    last_computed_at = Column(DateTime, nullable=True)
    example_count = Column(Integer, default=0)

class EnsembleStateModel(Base):
    __tablename__ = "ensemble_state"
    id = Column(String, primary_key=True)
    model_index = Column(Integer)
    action_id = Column(Integer)
    A_inv = Column(BLOB)
    b_vector = Column(BLOB)

class EnsemblePredictionLogModel(Base):
    __tablename__ = "ensemble_prediction_log"
    id = Column(String, primary_key=True)
    model_index = Column(Integer, index=True)
    action_id = Column(Integer)
    context_hash = Column(Integer)
    predicted_value = Column(Float)
    domain_id = Column(String, nullable=True)
    timestamp = Column(DateTime, server_default=sa.func.now())

class EnsembleAgreementSnapshotModel(Base):
    __tablename__ = "ensemble_agreement_snapshots"
    id = Column(String, primary_key=True)
    agreement_matrix = Column(BLOB)
    avg_pairwise = Column(Float)
    timestamp = Column(DateTime, server_default=sa.func.now())

class SessionStateModel(Base):
    __tablename__ = "session_state"
    id = Column(String, primary_key=True)
    session_id = Column(String, index=True)
    suggestions_paused = Column(Boolean, default=False)
    annotations_deferred = Column(JSON, default=list)
    notifications_deferred = Column(JSON, default=list)
    busy_session = Column(Boolean, default=False)
    created_at = Column(DateTime, server_default=sa.func.now())

class NoveltyTableModel(Base):
    __tablename__ = "novelty_tables"
    id = Column(String, primary_key=True)
    table_index = Column(Integer, index=True)
    hash_bucket = Column(String)
    visit_count = Column(Integer, default=0)
    last_updated = Column(DateTime, server_default=sa.func.now())

class NoveltyProjectionModel(Base):
    __tablename__ = "novelty_projections"
    id = Column(String, primary_key=True)
    table_index = Column(Integer)
    projection_matrix = Column(BLOB)

class ActionHistoryModel(Base):
    __tablename__ = "action_history"
    id = Column(String, primary_key=True)
    session_id = Column(String, index=True)
    action_name = Column(String, index=True)
    timestamp = Column(DateTime, server_default=sa.func.now())

class NoveltyHistoryModel(Base):
    __tablename__ = "novelty_history"
    id = Column(String, primary_key=True)
    session_id = Column(String, index=True)
    novelty_score = Column(Float)
    visit_count = Column(Integer)
    timestamp = Column(DateTime, server_default=sa.func.now())

class BlendedEdgeLogModel(Base):
    __tablename__ = "blended_edge_log"
    id = Column(String, primary_key=True)
    source_edge_a = Column(String)
    source_edge_b = Column(String)
    blended_edge_id = Column(String, nullable=True)
    accepted = Column(Boolean, default=False)
    timestamp = Column(DateTime, server_default=sa.func.now())

class CoSelectionLogModel(Base):
    __tablename__ = "co_selection_log"
    id = Column(String, primary_key=True)
    session_id = Column(String, index=True)
    primitive_a = Column(String)
    primitive_b = Column(String)
    timestamp = Column(DateTime, server_default=sa.func.now())


class TTLModel(Base):
    """Persisted EdgeTTL entries — a 'don't do this again'/'crash' mute must
    survive a sidecar restart, not just the current process."""
    __tablename__ = "ttl_entries"
    edge_id = Column(String, primary_key=True)
    expires_at = Column(String)
    cause = Column(String)
    set_at = Column(String)


class AppSettingsModel(Base):
    """Small settings KV store: ensemble-weight slider values and other
    user-tunable runtime settings that must survive restarts (rows 13/2)."""
    __tablename__ = "app_settings"
    key = Column(String, primary_key=True)
    value = Column(JSON, nullable=True)


# Columns added after a table's first release; `_ensure_column` ALTERs them
# in idempotently for databases created by older builds.
_LATE_COLUMNS = {
    "edges": [("override_rate", "FLOAT")],
    "domains": [("centroid", "BLOB")],
}


async def init_db(db_path: str):
    engine = create_async_engine(
        f"sqlite+aiosqlite:///{db_path}",
        echo=False,
        connect_args={"check_same_thread": False},
    )
    async with engine.begin() as conn:
        await conn.execute(sa.text("PRAGMA journal_mode=WAL"))
        await conn.execute(sa.text("PRAGMA synchronous=NORMAL"))
    async with engine.begin() as conn:
        await conn.run_sync(Base.metadata.create_all)
        for table, columns in _LATE_COLUMNS.items():
            result = await conn.execute(sa.text(f"PRAGMA table_info({table})"))
            existing = {row[1] for row in result}
            for name, ddl in columns:
                if name not in existing:
                    await conn.execute(
                        sa.text(f"ALTER TABLE {table} ADD COLUMN {name} {ddl}"))
    return engine
