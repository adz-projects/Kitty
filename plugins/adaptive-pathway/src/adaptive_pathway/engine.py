import json
import yaml
import numpy as np
import time
import uuid
from datetime import datetime
from collections import Counter
from pathlib import Path
import sqlalchemy as sa
from sqlalchemy.dialects.sqlite import insert as sqlite_upsert
from .storage.database import (init_db, EdgeModel, NodeModel, ActionHistoryModel,
    NoveltyHistoryModel, BlendedEdgeLogModel, CoSelectionLogModel, AnnotationModel,
    DomainModel, OverrideLogModel, PassiveTelemetryModel, FeedbackCentroidModel,
    EnsembleStateModel, EnsemblePredictionLogModel, EnsembleAgreementSnapshotModel,
    TTLModel, AppSettingsModel)
from .storage.tiered import TieredCache
from .storage.vec import VectorIndex
from .decision.ensemble import BootstrapEnsemble
from .decision.in_session import InSessionBandit
from .decision.novelty import CountBasedNovelty
from .decision.selector import ActionSelector
from .learning.curiosity import CuriosityNudge
from .learning.preferences import PreferenceDetector
from .learning.ttl import EdgeTTL
from .learning.bleed import DomainBleed
from .features import FeatureHasher, ActionBucketer
from .types import (EdgeStatus, PrimitiveSource, EdgeInfo, SessionState,
                     SchismAlert, SchismState, DecisionResult, AnnotationType)
from .discovery import PrimitiveDiscoverer, DomainDiscovery
from .health import HealthChecker
from .embeddings import EmbeddingProvider

import struct


def _pack_matrix(arr):
    """float64 ndarray -> (rank/shape int32 prefix) + raw bytes.

    Row 10 of 82inefficiencies.md: Thompson matrices used to be persisted as
    JSON text (~40-60KB each); binary is ~16KB for a 64x64 float64 matrix
    and needs no float->string round trip."""
    arr = np.asarray(arr, dtype=np.float64)
    shape_buf = struct.pack(f"=i{len(arr.shape)}i", len(arr.shape), *arr.shape)
    return shape_buf + arr.tobytes()


def _unpack_matrix(raw):
    """Inverse of `_pack_matrix`; returns None on any malformed input."""
    try:
        rank = struct.unpack_from("=i", raw, 0)[0]
        if not (0 < rank <= 2):
            return None
        shape = struct.unpack_from(f"={rank}i", raw, 4)
        offset = 4 + 4 * rank
        arr = np.frombuffer(raw, dtype=np.float64, offset=offset)
        if arr.size == 0 or arr.size != int(np.prod(shape)):
            return None
        return arr.copy().reshape(shape)
    except (struct.error, ValueError):
        return None


def _decode_state_blob(raw):
    """Decode a persisted A_inv/b blob: binary format first, then the legacy
    JSON-text format (databases written before the binary switch). Returns a
    float64 ndarray, or None if the blob is unreadable."""
    if isinstance(raw, (bytes, bytearray)):
        arr = _unpack_matrix(raw)
        if arr is not None:
            return arr
        try:
            parsed = json.loads(bytes(raw).decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return None
    else:
        parsed = raw
    return np.asarray(parsed, dtype=np.float64)

class AdaptivePathway:
    def __init__(self, db_path="./pathway.db", config_path=None, **overrides):
        default = Path(__file__).parent / "config" / "defaults.yaml"
        with open(config_path or default) as f:
            self.config = yaml.safe_load(f)
        for key, value in overrides.items():
            self._deep_update(self.config, key, value)

        self.db_path = db_path
        self._engine = None
        self._vec_index = VectorIndex()
        self._hasher = FeatureHasher(self.config["thompson"]["feature_buckets"])
        self._bucketer = ActionBucketer(self.config["thompson"]["max_action_buckets"])
        self._tiered = TieredCache(self.config, self._vec_index, self._bucketer)
        self._ensemble = BootstrapEnsemble(self.config, self._get_domain, self._get_edge)
        self._novelty = CountBasedNovelty(self.config)
        self._nudge = CuriosityNudge(self.config, novelty=self._novelty)
        self._detector = PreferenceDetector(self.config)
        self._ttl = EdgeTTL(self.config)
        self._bleed = DomainBleed(self.config)
        self._embedder = EmbeddingProvider(self.config)
        self._sessions = {}
        self._edge_index = {}
        self._centroids = None
        self._warm_ready = False
        self._action_history = []
        self._novelty_history = []
        self._utilization_counter = 0
        self._annotations_cache = []
        self._domains_cache = {}
        self._attribution_log = {}
        self._novelty_lambda_boost = 0.0
        self._novelty_lambda_boost_sessions_remaining = 0
        self._embed_decode_failures = 0
        self._ttl_dirty: set[str] = set()
        self._primitive_discoverer = PrimitiveDiscoverer(
            self.config, self._get_edges, self._bucketer)
        self._domain_discovery = DomainDiscovery(
            self.config, self._get_edge)
        self._health_checker = HealthChecker(
            self.config, self.get_state, self._get_all_edges, self._get_novelty_values,
            lambda: self._ensemble, self.list_domains, self._hasher, self._detector)

    async def _ensure_db(self):
        if self._engine is None:
            self._engine = await init_db(self.db_path)

    async def session_open(self, session_id, mode="thought_partner",
                           domain_hint=None):
        await self._ensure_db()
        if not self._warm_ready:
            await self._warm_data()
            self._warm_ready = True
        in_session = InSessionBandit(self.config)
        selector = ActionSelector(
            self._ensemble, in_session, self._novelty, self.config,
            self._get_domain, self._get_edges, self._bucketer, self._hasher,
            action_history=self._action_history,
            novelty_history=self._novelty_history,
            bleed=self._bleed, ttl=self._ttl,
        )
        selector.set_nudge(self._nudge)
        selector.set_context(domain_hint=domain_hint, suggestions_paused=False, mode=mode)
        state = SessionState(session_id=session_id, mode=mode,
                            domain_hint=domain_hint,
                            in_session=in_session, selector=selector,
                            opened_at=time.strftime("%Y-%m-%dT%H:%M:%SZ"))
        self._sessions[session_id] = state
        self._domain_discovery.increment_session()
        return state

    async def session_close(self, session_id):
        state = self._sessions.pop(session_id, None)
        if state is None:
            return
        if state.annotations_deferred:
            for ann in state.annotations_deferred:
                await self.record_annotation(session_id, ann, paused_replay=True)
        if state.in_session:
            state.in_session.reset()
        if state.co_selected and self._engine:
            async with self._engine.begin() as conn:
                for prim_a, others in state.co_selected.items():
                    for prim_b in others:
                        await conn.execute(sa.insert(CoSelectionLogModel).values(
                            id=str(uuid.uuid4()), session_id=session_id,
                            primitive_a=prim_a, primitive_b=prim_b))

        if self._novelty_lambda_boost_sessions_remaining > 0:
            self._novelty_lambda_boost_sessions_remaining -= 1
            if self._novelty_lambda_boost_sessions_remaining <= 0:
                self._novelty_lambda_boost = 0.0

        cluster_center = self._domain_discovery.estimate_centroids_from_pool()
        if cluster_center is not None:
            new_id = f"auto_domain_{self._domain_discovery.domain_count}"
            if self._domain_discovery.add_domain(new_id, new_id):
                self._domain_discovery.update_domain_centroid(new_id, cluster_center)
                self._domain_discovery.clear_unassigned_pool()

    async def _warm_data(self):
        async with self._engine.begin() as conn:
            result = await conn.execute(
                sa.select(EdgeModel).where(
                    sa.or_(EdgeModel.tier == "hot", EdgeModel.tier == "warm")))
            edges = []
            for row in result:
                edge = EdgeInfo(id=row.id, semantic_primitive=row.semantic_primitive,
                               domain_id=row.domain_id or "", domain=row.domain_id or "",
                               confidence=row.confidence or 0.5,
                               status=EdgeStatus(row.status or "provisional"),
                               tier=row.tier or "hot",
                               source=PrimitiveSource(row.primitive_source or "auto_named"),
                               frequency=row.frequency or 0,
                               co_selected_with=row.co_selected_with or [],
                               last_accessed=str(row.last_accessed) if row.last_accessed else "",
                               created_at=str(row.created_at) if row.created_at else "")
                bucket = self._bucketer.get_bucket(row.semantic_primitive)
                self._edge_index.setdefault(bucket, []).append(edge)
                edges.append(edge)
            result = await conn.execute(
                sa.select(EdgeModel.id, NodeModel.context_embedding)
                .join(NodeModel, EdgeModel.source_node_id == NodeModel.id)
                .where(EdgeModel.tier.in_(["hot", "warm"])))
            # Row 5 of 82inefficiencies.md: a stored embedding whose byte
            # length doesn't match embedding_dim (e.g. written by a build
            # with a different model) must be skipped, not fed into the
            # vector index — a mismatched vector in the DPP kernel/novelty
            # path distorts every similarity it touches.
            warm = []
            for r in result:
                if r.context_embedding is None:
                    continue
                emb = np.frombuffer(r.context_embedding, dtype=np.float32)
                if emb.size != self.config["embedding_dim"]:
                    continue
                warm.append((r.id, emb))
            embedding_by_id = dict(warm)
            for edge in edges:
                emb = embedding_by_id.get(edge.id)
                if emb is not None:
                    edge.embedding = emb
            self._tiered.warm_from_db(edges, warm)
            result = await conn.execute(
                sa.select(ActionHistoryModel.action_name)
                .order_by(ActionHistoryModel.timestamp.desc()).limit(2000))
            rows = [r.action_name for r in result]
            self._action_history = rows[::-1]
            result = await conn.execute(
                sa.select(NoveltyHistoryModel.novelty_score)
                .order_by(NoveltyHistoryModel.timestamp.desc()).limit(500))
            rows = [r.novelty_score for r in result]
            self._novelty_history = rows[::-1]

            self._restore_ensemble_state(await conn.execute(sa.select(EnsembleStateModel)))

            # Warm-restore TTL entries so mutes survive restart (row 2).
            ttl_rows = await conn.execute(sa.select(TTLModel))
            ttl_entries = {}
            for r in ttl_rows:
                ttl_entries[r.edge_id] = {
                    "expires_at": r.expires_at,
                    "cause": r.cause,
                    "set_at": r.set_at,
                }
            self._ttl.restore(ttl_entries)

            # Warm-restore domains (row 3): DomainDiscovery._domains must
            # survive restart so inferred domain assignments remain stable.
            domain_rows = await conn.execute(sa.select(DomainModel))
            domain_data = {}
            for r in domain_rows:
                domain_data[r.id] = {
                    "name": r.name,
                    "source": r.domain_source,
                    "edge_count": r.edge_count or 0,
                    "last_inferred": r.last_inferred,
                    "locked": bool(r.locked),
                    "centroid": r.centroid,
                }
            self._domain_discovery.from_dict(domain_data)

            # Warm-restore preference detector centroids (row 15).
            fc_rows = await conn.execute(
                sa.select(FeedbackCentroidModel).where(
                    FeedbackCentroidModel.id == "default"))
            fc_row = fc_rows.first()
            if fc_row and fc_row.example_count and fc_row.example_count > 0:
                if fc_row.positive_centroid is not None and len(fc_row.positive_centroid) > 0:
                    self._detector._positive_centroid = np.frombuffer(
                        fc_row.positive_centroid, dtype=np.float64).copy()
                if fc_row.negative_centroid is not None and len(fc_row.negative_centroid) > 0:
                    self._detector._negative_centroid = np.frombuffer(
                        fc_row.negative_centroid, dtype=np.float64).copy()
                self._detector._example_count = fc_row.example_count
                if fc_row.last_computed_at is not None:
                    self._detector._last_computed_at = fc_row.last_computed_at.timestamp()

            # Warm-restore ensemble weights from settings KV (row 13).
            sw_rows = await conn.execute(
                sa.select(AppSettingsModel).where(
                    AppSettingsModel.key == "ensemble_weights"))
            sw_row = sw_rows.first()
            if sw_row and sw_row.value:
                w = sw_row.value
                if isinstance(w, dict):
                    if "ig_weight_min" in w:
                        self._ensemble.ig_weight_min = w["ig_weight_min"]
                        self.config["ensemble"]["ig_weight_min"] = w["ig_weight_min"]
                    if "ig_weight_max" in w:
                        self._ensemble.ig_weight_max = w["ig_weight_max"]
                        self.config["ensemble"]["ig_weight_max"] = w["ig_weight_max"]
                    if "pc_weight" in w:
                        self._ensemble.pc_weight = w["pc_weight"]
                        self.config["ensemble"]["pc_weight"] = w["pc_weight"]

    def _restore_ensemble_state(self, rows):
        """Reload learned Thompson-sampling weights persisted by
        `_persist_ensemble_state`. Rows with a model_index/action_id/shape
        that no longer matches the current config (e.g. `feature_buckets` or
        `max_action_buckets` changed since the row was written) are skipped
        rather than applied — a stale-shaped `A_inv` would silently corrupt
        every prediction for that bucket instead of just starting fresh.
        """
        d = self._ensemble.d_features
        n_actions = self._ensemble.n_actions
        for row in rows:
            if row.model_index not in self._LEARNABLE_MODEL_INDICES:
                continue
            if not (0 <= row.action_id < n_actions):
                continue
            if row.A_inv is None or row.b_vector is None:
                continue
            a_inv = _decode_state_blob(row.A_inv)
            b = _decode_state_blob(row.b_vector)
            if a_inv is None or b is None:
                continue
            if a_inv.shape != (d, d) or b.shape != (d,):
                continue
            self._ensemble.models[row.model_index].set_state(
                row.action_id, {"A_inv": a_inv.tolist(), "b": b.tolist()})

    def decide(self, session_id, context_embedding, available_actions):
        if not self._warm_ready:
            return DecisionResult(hints=[], confidence=0.5, novelty=1.0,
                                 attribution_ids=[], is_flow_state=False)
        state = self._sessions.get(session_id)
        if state is None:
            raise ValueError(f"Unknown session: {session_id}")
        if self._ensemble.schism_state == SchismState.REVIEWING:
            return DecisionResult(hints=[], confidence=0.5, novelty=0.0,
                                 attribution_ids=[], is_flow_state=False)

        self._detector.tick_pending()

        domain_id = state.domain_hint
        if not domain_id:
            ctx_raw_for_domain = np.asarray(context_embedding, dtype=np.float32).ravel()
            edges_for_domain = self._get_edges(available_actions)
            domain_id = self._domain_discovery.infer_domain(
                ctx_raw_for_domain, available_actions, edges_for_domain)
        # record_outcome has no domain input of its own — edges created there
        # inherit the domain inferred here on the same session's latest turn.
        state.last_domain_id = domain_id

        state.selector.set_novelty_lambda_boost(self._novelty_lambda_boost)
        # `SessionState` has no `action_history` of its own — action history
        # is tracked engine-wide (`self._action_history`, same list already
        # fed to `selector.set_context` above); every `decide()` call was
        # crashing with an AttributeError until this used the right list.
        self._nudge.check_and_trigger(self._action_history, mode=state.mode)
        result = state.selector.select(
            session_id=session_id,
            context_embedding=context_embedding,
            available_actions=available_actions,
            domain_id=domain_id,
            suggestions_paused=state.suggestions_paused,
        )

        state.last_hints = result.hints[:]
        for h in result.hints:
            if getattr(h, "source_model", "standard") == "wildcard":
                state.wildcard_count += 1
            elif getattr(h, "source_model", "standard") == "uncertain":
                state.uncertainty_slot_count += 1
            self._attribution_log[h.attribution_id] = {
                "edge_id": getattr(h, "edge_id", None),
                "primitive": getattr(h, "primitive", None) or getattr(h, "source_primitive_a", None),
                "rationale": getattr(h, "rationale", None),
                "source_model": getattr(h, "source_model", "standard"),
                "session_id": session_id,
                "domain": getattr(h, "domain", None),
                "timestamp": time.time(),
            }
            if len(self._attribution_log) > 2000:
                oldest_key = next(iter(self._attribution_log))
                del self._attribution_log[oldest_key]

        primitives = []
        for h in result.hints:
            if hasattr(h, "primitive"):
                primitives.append(h.primitive)
            elif hasattr(h, "source_primitive_a"):
                primitives.append(h.source_primitive_a)
        for i, pa in enumerate(primitives):
            for pb in primitives[i + 1:]:
                state.co_selected.setdefault(pa, set()).add(pb)
                state.co_selected.setdefault(pb, set()).add(pa)

        if self._ensemble.schism_state == SchismState.DETECTED:
            sd = self._ensemble.schism_data
            result.schism_alert = SchismAlert(
                faction_a=sd["fa"], faction_b=sd["fb"],
                within_a=sd["wa"], within_b=sd["wb"],
                between=sd["bt"],
                faction_a_models=len(sd["fa"]),
                faction_b_models=len(sd["fb"]),
                detected_at=time.strftime("%Y-%m-%dT%H:%M:%SZ"))
        return result

    # Only models 0-4 (the 4 standard Thompson models + the IG model) carry
    # learned per-action-bucket weights (`A_inv`/`b`) — model 5 (Paradigm
    # Challenge) is a pure scoring function with no learned state to save.
    _LEARNABLE_MODEL_INDICES = (0, 1, 2, 3, 4)

    async def _persist_ensemble_state(self, conn, bucket):
        """Upsert the learned Thompson-sampling weights for one action bucket
        into ``ensemble_state``, within the caller's already-open transaction.

        Without this, `decide()`'s in-memory `A_inv`/`b` matrices — the
        actual learned preferences — never survive a process restart, even
        though `action_history`/`novelty_history` keep accumulating rows and
        make the database look like it's learning.
        """
        rows = []
        for model_index in self._LEARNABLE_MODEL_INDICES:
            model = self._ensemble.models[model_index]
            state = model.get_state(bucket)
            rows.append({
                "id": f"{model_index}:{bucket}",
                "model_index": model_index,
                "action_id": bucket,
                "A_inv": _pack_matrix(state["A_inv"]),
                "b_vector": _pack_matrix(state["b"]),
            })
        if not rows:
            return
        stmt = sqlite_upsert(EnsembleStateModel).values(rows)
        stmt = stmt.on_conflict_do_update(
            index_elements=["id"],
            set_={"A_inv": stmt.excluded.A_inv, "b_vector": stmt.excluded.b_vector},
        )
        await conn.execute(stmt)

    async def _sync_ttl_store(self, conn):
        """Flush dirty EdgeTTL entries to `ttl_entries` (row 2 of
        82inefficiencies.md): a crash TTL or 30-day user_rejected mute must
        survive a sidecar restart. Entries removed from memory are deleted
        from the table; the store is small, so a targeted per-id sync is
        cheap. Expired rows not re-checked in memory are pruned by
        run_maintenance."""
        if not self._ttl_dirty:
            return
        snapshot = self._ttl.snapshot()
        dirty = self._ttl_dirty
        self._ttl_dirty = set()
        for edge_id in dirty:
            entry = snapshot.get(edge_id)
            if entry is None:
                await conn.execute(
                    sa.delete(TTLModel).where(TTLModel.edge_id == edge_id))
            else:
                stmt = sqlite_upsert(TTLModel).values(
                    edge_id=edge_id,
                    expires_at=entry["expires_at"],
                    cause=entry["cause"],
                    set_at=entry.get("set_at", entry["expires_at"]),
                )
                stmt = stmt.on_conflict_do_update(
                    index_elements=["edge_id"],
                    set_={
                        "expires_at": stmt.excluded.expires_at,
                        "cause": stmt.excluded.cause,
                        "set_at": stmt.excluded.set_at,
                    },
                )
                await conn.execute(stmt)

    async def _sync_domains(self, conn):
        """Persist all DomainDiscovery domains to `domains` table (row 3).
        Wipe+reinsert is used since the domain set is tiny (≤ max_domains=8)."""
        await conn.execute(sa.delete(DomainModel))
        for did, info in self._domain_discovery.to_dict().items():
            source_val = info.get("source", "auto_named")
            if hasattr(source_val, "value"):
                source_val = source_val.value
            elif not isinstance(source_val, str):
                source_val = str(source_val)
            await conn.execute(sqlite_upsert(DomainModel).values(
                id=did,
                name=info.get("name", did),
                domain_source=source_val,
                edge_count=int(info.get("edge_count", 0)),
                last_inferred=info.get("last_inferred"),
                locked=bool(info.get("locked", False)),
                centroid=info.get("centroid"),
            ).on_conflict_do_update(
                index_elements=["id"],
                set_={
                    "name": sqlite_upsert(DomainModel).excluded.name,
                    "domain_source": sqlite_upsert(DomainModel).excluded.domain_source,
                    "edge_count": sqlite_upsert(DomainModel).excluded.edge_count,
                    "last_inferred": sqlite_upsert(DomainModel).excluded.last_inferred,
                    "locked": sqlite_upsert(DomainModel).excluded.locked,
                    "centroid": sqlite_upsert(DomainModel).excluded.centroid,
                },
            ))

    def _touch_edge(self, primitive, ctx_raw, domain_id, reward):
        """In-memory upsert of the graph edge for one semantic primitive.

        This (plus `_persist_edge` below) is what actually populates the
        `edges`/`nodes` tables — before it existed, nothing anywhere wrote
        them, so `list_edges`/`list_domains` stayed empty forever no matter
        how much the extension was used (domains are derived from edges'
        `domain_id`, so they were empty for the same reason).

        Returns ``(edge, created)``.
        """
        primitive = str(primitive)
        bucket = self._bucketer.get_bucket(primitive)
        now = time.strftime("%Y-%m-%dT%H:%M:%SZ")
        r = max(-1.0, min(1.0, float(reward)))
        for edge in self._edge_index.get(bucket, []):
            if edge.semantic_primitive == primitive:
                edge.frequency = (edge.frequency or 0) + 1
                edge.last_accessed = now
                step = self.config["confidence"]["base_step"]
                edge.confidence = max(0.05, min(0.99, (edge.confidence or 0.5) + step * r))
                if (edge.status == EdgeStatus.PROVISIONAL
                        and edge.frequency >= self.config["cooldown"]["provisional_successes"]
                        and edge.confidence > 0.5):
                    edge.status = EdgeStatus.ESTABLISHED
                if domain_id and not edge.domain_id:
                    edge.domain_id = domain_id
                    edge.domain = domain_id
                return edge, False
        edge = EdgeInfo(
            id=str(uuid.uuid4()), semantic_primitive=primitive,
            domain_id=domain_id or "default", domain=domain_id or "default",
            confidence=0.5, status=EdgeStatus.PROVISIONAL, tier="hot",
            source=PrimitiveSource.AUTO_NAMED, frequency=1,
            created_at=now, last_accessed=now,
            embedding=(np.array(ctx_raw, dtype=np.float32).ravel().copy()
                       if (ctx_raw is not None and not np.allclose(ctx_raw, 0.0, atol=1e-6))
                       else None),
        )
        self._edge_index.setdefault(bucket, []).append(edge)
        # Also into the tiered cache so `_get_edges` (the selector's hint
        # source) sees it this session, not only after the next warm load.
        self._tiered.add_hot(edge)
        return edge, True

    async def _persist_edge(self, conn, edge, created):
        """Write one touched edge through to SQLite, within the caller's
        already-open transaction. New edges also get a node row carrying the
        context embedding (what `_warm_data`'s join feeds the vector index)."""
        node_id = None
        if created and edge.embedding is not None:
            node_id = str(uuid.uuid4())
            await conn.execute(sa.insert(NodeModel).values(
                id=node_id,
                context_embedding=np.asarray(edge.embedding, dtype=np.float32).tobytes(),
                status="provisional",
            ))
        # Upsert (not insert-if-created / update-if-not): also self-heals an
        # edge that exists in memory but has no row yet (e.g. created by an
        # older build of this code that never persisted it).
        stmt = sqlite_upsert(EdgeModel).values(
            id=edge.id,
            source_node_id=node_id,
            semantic_primitive=edge.semantic_primitive,
            confidence=float(edge.confidence or 0.5),
            domain_id=edge.domain_id or "default",
            tier=edge.tier or "hot",
            status=edge.status.value,
            primitive_source=edge.source.value,
            frequency=int(edge.frequency or 1),
            tags=edge.tags or [],
            domain_tags=edge.domain_tags or [],
            co_selected_with=edge.co_selected_with or [],
            override_rate=float(edge.override_rate or 0.0),
        )
        # Row 3 of 82inefficiencies.md: the update SET used to omit
        # semantic_primitive/tier/tags/domain_tags/co_selected_with, so edge
        # renames, tier changes and annotation labels silently reverted to
        # their stale DB values on restart.
        set_ = {
            "semantic_primitive": stmt.excluded.semantic_primitive,
            "confidence": stmt.excluded.confidence,
            "domain_id": stmt.excluded.domain_id,
            "tier": stmt.excluded.tier,
            "status": stmt.excluded.status,
            "primitive_source": stmt.excluded.primitive_source,
            "frequency": stmt.excluded.frequency,
            "tags": stmt.excluded.tags,
            "domain_tags": stmt.excluded.domain_tags,
            "co_selected_with": stmt.excluded.co_selected_with,
            "override_rate": stmt.excluded.override_rate,
            "last_accessed": sa.func.now(),
        }
        # Only back-fill the node link when this call created the node row;
        # a later update must not null out the existing source_node_id.
        if node_id is not None:
            set_["source_node_id"] = stmt.excluded.source_node_id
        stmt = stmt.on_conflict_do_update(index_elements=["id"], set_=set_)
        await conn.execute(stmt)

    async def record_outcome(self, session_id, action_id, reward,
                             context_embedding, is_blended=False,
                             blend_edge_ids=None, error_type=None):
        state = self._sessions.get(session_id)
        paused = state.suggestions_paused if state else False
        weight = self.config["pause"]["learning_weight"] if paused else 1.0
        ctx_raw = np.asarray(context_embedding, dtype=np.float32).ravel()
        ctx_features = np.asarray(self._hasher.hash_embedding(ctx_raw), dtype=np.float64)
        self._health_checker.update_metrics(action_id, reward)

        touched_buckets = set()
        touched_edges = []
        edge_domain = (state.domain_hint or state.last_domain_id) if state else None

        if is_blended and blend_edge_ids:
            for edge_id in blend_edge_ids:
                r = (reward / len(blend_edge_ids)) * weight
                bucket = self._bucketer.get_bucket(str(edge_id))
                touched_buckets.add(bucket)
                self._ensemble.update(bucket, ctx_features, r)
                if state and state.in_session and (
                    not paused or self.config["in_session"]["learn_during_pause"]):
                    state.in_session.update(bucket, ctx_features, r)
                # blend_edge_ids reference existing edges by id — bump the
                # real edge's primitive when it resolves, else treat the id
                # itself as a primitive so the graph still gains a node.
                existing = self.get_edge(str(edge_id))
                primitive = existing.semantic_primitive if existing else str(edge_id)
                touched_edges.append(self._touch_edge(primitive, ctx_raw, edge_domain, r))
            action_name = f"blended:{'+'.join(blend_edge_ids)}"
            if self._engine:
                async with self._engine.begin() as conn:
                    await conn.execute(sa.insert(BlendedEdgeLogModel).values(
                        id=str(uuid.uuid4()),
                        source_edge_a=blend_edge_ids[0],
                        source_edge_b=blend_edge_ids[1] if len(blend_edge_ids) > 1 else blend_edge_ids[0],
                        blended_edge_id=f"blended:{blend_edge_ids[0]}+{blend_edge_ids[1] if len(blend_edge_ids) > 1 else blend_edge_ids[0]}",
                        accepted=reward > 0,
                    ))
        else:
            reward_weighted = reward * weight
            bucket = self._bucketer.get_bucket(str(action_id))
            touched_buckets.add(bucket)
            self._ensemble.update(bucket, ctx_features, reward_weighted)
            if state and state.in_session and (
                not paused or self.config["in_session"]["learn_during_pause"]):
                state.in_session.update(bucket, ctx_features, reward_weighted)
            action_name = str(action_id)
            touched_edges.append(
                self._touch_edge(action_name, ctx_raw, edge_domain, reward_weighted))

        self._action_history.append(action_name)
        if len(self._action_history) > 2000:
            self._action_history.pop(0)

        novelty_score = float(self._novelty.current_score(ctx_raw))
        self._novelty_history.append(novelty_score)
        if len(self._novelty_history) > 500:
            self._novelty_history.pop(0)

        if error_type == "crash" and not is_blended:
            self._ttl.record_error(str(action_id), "crash")
            self._ttl_dirty.add(str(action_id))

        last_hints = state.last_hints if state else []

        # Track user-initiated exploration
        last_action_ids = set()
        for h in last_hints:
            aid = getattr(h, "primitive", "")
            if not aid:
                aid = getattr(h, "source_primitive_a", "")
            last_action_ids.add(aid)
        if action_name and not is_blended and action_name not in last_action_ids:
            self._novelty.record_user_action(str(action_id))

        # Exploration-success reinforcement
        if reward > 0.5:
            for h in last_hints:
                if getattr(h, "source_model", None) == "wildcard":
                    domain_id = getattr(h, "domain", "") or "default"
                    self._novelty.bump_domain_lambda(domain_id, 0.02)

        if state and state.domain_hint:
            self._domain_discovery.update_domain_centroid(state.domain_hint, ctx_raw)

        observed_actions = blend_edge_ids if (is_blended and blend_edge_ids) else [str(action_id)]
        self._primitive_discoverer.maybe_discover(session_id, ctx_raw, observed_actions)
        if self._primitive_discoverer._call_counter % self._primitive_discoverer.call_interval == 0:
            for edge in self._get_all_edges():
                related = [name for name, _ in
                          self._primitive_discoverer.get_co_occurrence(edge.semantic_primitive, top_k=10)]
                if related:
                    edge.co_selected_with = sorted(set((edge.co_selected_with or []) + related))

        if self._engine:
            async with self._engine.begin() as conn:
                await conn.execute(sa.insert(ActionHistoryModel).values(
                    id=str(uuid.uuid4()), session_id=session_id,
                    action_name=action_name))
                await conn.execute(sa.insert(NoveltyHistoryModel).values(
                    id=str(uuid.uuid4()), session_id=session_id,
                    novelty_score=novelty_score, visit_count=1))
                for bucket in touched_buckets:
                    await self._persist_ensemble_state(conn, bucket)
                for edge, created in touched_edges:
                    await self._persist_edge(conn, edge, created)
                await self._sync_ttl_store(conn)

    async def record_error(self, edge_id, error_type):
        self._ttl.record_error(edge_id, error_type)
        self._ttl_dirty.add(str(edge_id))
        if self._engine:
            async with self._engine.begin() as conn:
                await self._sync_ttl_store(conn)

    async def run_maintenance(self):
        dc = self.config["decay"]
        self._ensemble.apply_confidence_decay(
            dc["base_half_life_hours"], dc["half_life_multiplier"],
            dc.get("max_decay_fraction", 0.3))

        if not self._engine:
            return
        cfg = self.config
        # Retention is measured in ROWS (turns), not seconds: keep the most
        # recent N history rows so cross-session learning history survives.
        retention_rows = (cfg["plateau_risk"]["entropy_window"] *
                          cfg["plateau_risk"]["entropy_stride"] * 10)
        async with self._engine.begin() as conn:
            await conn.execute(
                sa.text(
                    "DELETE FROM action_history WHERE id NOT IN "
                    "(SELECT id FROM action_history "
                    "ORDER BY timestamp DESC, id DESC LIMIT :n)"
                ),
                {"n": retention_rows},
            )
            await conn.execute(
                sa.text(
                    "DELETE FROM novelty_history WHERE id NOT IN "
                    "(SELECT id FROM novelty_history "
                    "ORDER BY timestamp DESC, id DESC LIMIT :n)"
                ),
                {"n": retention_rows},
            )
            cold_threshold = int(time.time() - (cfg["tiers"]["cold_archive_days"] * 86400))
            cold_dt = sa.text(f"datetime({cold_threshold}, 'unixepoch')")
            await conn.execute(
                sa.update(EdgeModel).where(
                    sa.and_(EdgeModel.tier == "warm",
                           EdgeModel.last_accessed < cold_dt)
                ).values(tier="cold"))
            await conn.execute(
                sa.delete(EdgeModel).where(
                    sa.and_(EdgeModel.tier == "cold",
                           EdgeModel.last_accessed < cold_dt)))
            centroid_stale_days = cfg["health"]["centroid_stale_days"]
            if centroid_stale_days > 0:
                centroid_threshold = int(time.time() - (centroid_stale_days * 86400))
                centroid_dt = sa.text(f"datetime({centroid_threshold}, 'unixepoch')")
                await conn.execute(
                    sa.delete(FeedbackCentroidModel).where(
                        FeedbackCentroidModel.last_computed_at < centroid_dt))
            # Prune expired TTL entries (is_expired drops them from memory lazily).
            await conn.execute(
                sa.delete(TTLModel).where(
                    TTLModel.expires_at <= datetime.utcnow().isoformat()))
            await self._sync_domains(conn)
            await conn.execute(sa.text("PRAGMA optimize"))

    async def record_annotation(self, session_id, annotation, paused_replay=False):
        annotation_type = annotation.get("type") if isinstance(annotation, dict) else annotation
        context_embedding = annotation.get("context_embedding") if isinstance(annotation, dict) else None
        edge_id = annotation.get("edge_id") if isinstance(annotation, dict) else None
        action_id = annotation.get("action_id") if isinstance(annotation, dict) else None
        intensity = annotation.get("intensity", 0.5) if isinstance(annotation, dict) else 0.5

        state = self._sessions.get(session_id)
        paused = state.suggestions_paused if state else False
        weight = self.config["pause"]["learning_weight"] if paused else 1.0
        # Must be defined even when the block below doesn't run (edge_id
        # present but no context_embedding — e.g. Kitty's copy-button
        # micro_positive call, which passes neither `context_embedding` nor
        # `context`) — both are read again after the block, in the
        # AnnotationModel insert.
        bucket = None
        reward_weight = 0.0

        if edge_id and context_embedding is not None:
            ctx_raw = np.asarray(context_embedding, dtype=np.float32).ravel()
            ctx_features = np.asarray(self._hasher.hash_embedding(ctx_raw), dtype=np.float64)
            bucket = self._bucketer.get_bucket(str(edge_id))

            if annotation_type in (AnnotationType.KEEP_THIS, "keep_this"):
                detection = self._detector.detect(ctx_raw, edge_id=edge_id)
                if detection["type"] is not None:
                    reward_weight = detection["reward_weight"]
                else:
                    reward_weight = self.config["preferences"]["keep_this_weight_mild"]
                self._detector.add_labeled_example(ctx_raw, "keep_this", intensity, edge_id=edge_id)

            elif annotation_type in (AnnotationType.DONT_DO_AGAIN, "dont_do_again"):
                detection = self._detector.detect(ctx_raw, edge_id=edge_id)
                if detection["type"] is not None:
                    reward_weight = detection["reward_weight"]
                else:
                    reward_weight = self.config["preferences"]["dont_do_again_weight_mild"]
                self._detector.add_labeled_example(ctx_raw, "dont_do_again", intensity, edge_id=edge_id)
                if intensity >= self.config["preferences"]["intensity_moderate"]:
                    lambda_boost = self._detector.get_lambda_boost("dont_do_again", detection.get("intensity"))
                    sessions = lambda_boost.get("sessions", 0)
                    if sessions > 0:
                        self._novelty_lambda_boost_sessions_remaining = max(
                            self._novelty_lambda_boost_sessions_remaining, sessions)
                        self._novelty_lambda_boost = min(self._novelty_lambda_boost + 0.1, 0.3)
                    # A moderate+ rejection is a topic-level "stop suggesting
                    # this" signal, not just a confidence nudge — suppress
                    # the edge from hints outright for a month via the same
                    # TTL machinery used for tool errors, rather than
                    # relying on slow confidence decay to eventually bury it.
                    self._ttl.set_ttl(str(edge_id), "user_rejected")
                    self._ttl_dirty.add(str(edge_id))

            elif annotation_type in (AnnotationType.MICRO_POSITIVE, "micro_positive"):
                reward_weight = self._clamp_micro_reward(
                    state, self.config["telemetry"]["save_event"] * 0.5)
            elif annotation_type in (AnnotationType.MICRO_NEGATIVE, "micro_negative"):
                reward_weight = self._clamp_micro_reward(
                    state, -self.config["telemetry"]["save_event"] * 0.3)
            elif annotation_type in (AnnotationType.EXPLORE_ALTERNATIVE, "explore_alternative"):
                self._nudge.trigger("User requested exploration", state.mode if state else "thought_partner")
            elif annotation_type in (AnnotationType.RETRY_SAME_INTENT, "retry_same_intent"):
                reward_weight = self.config["telemetry"]["immediate_followup"]

            if abs(reward_weight) > 1e-10:
                self._ensemble.update(bucket, ctx_features, reward_weight * weight)
                if state and state.in_session and (
                    not paused_replay or self.config["in_session"]["learn_during_pause"]):
                    state.in_session.update(bucket, ctx_features, reward_weight * weight)

        # An annotation's edge_id is a semantic label (e.g.
        # "style:critique:structural") the model coins on the fly — this is
        # where style edges are born, since they never pass through
        # record_outcome. Touch the graph even for reward-neutral annotation
        # types: the user referenced the label, so it should exist.
        touched_edge = None
        if edge_id:
            ctx_for_edge = (np.asarray(context_embedding, dtype=np.float32).ravel()
                            if context_embedding is not None else None)
            edge_domain = (state.domain_hint or state.last_domain_id) if state else None
            touched_edge = self._touch_edge(edge_id, ctx_for_edge, edge_domain, reward_weight)

        if self._engine and edge_id:
            ann_id = str(uuid.uuid4())
            ann_entry = {
                "id": ann_id,
                "edge_id": edge_id,
                "annotation_type": str(annotation_type),
                "intensity": float(intensity),
                "detection_confidence": 0.5,
                "detection_method": "heuristic",
                "behavioral_confirmation": False,
                "multi_turn_resolved": False,
                "session_id": session_id,
                "action_id": action_id,
                "reward_weight": reward_weight,
                "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            }
            self._annotations_cache.append(ann_entry)
            if len(self._annotations_cache) > 1000:
                self._annotations_cache.pop(0)
            async with self._engine.begin() as conn:
                await conn.execute(sa.insert(AnnotationModel).values(
                    id=ann_id,
                    edge_id=edge_id,
                    annotation_type=str(annotation_type),
                    intensity=float(intensity),
                    detection_confidence=0.5,
                    detection_method="heuristic",
                    session_id=session_id,
                    action_id=action_id,
                    reward_weight=reward_weight,
                ))
                if bucket is not None and abs(reward_weight) > 1e-10:
                    await self._persist_ensemble_state(conn, bucket)
                if touched_edge is not None:
                    await self._persist_edge(conn, *touched_edge)
                await self._sync_ttl_store(conn)
                # Persist preference centroids (row 15): detector state must
                # survive restart so "keep this"/"don't do again" preferences
                # remain effective across sidecar launches.
                if self._detector._example_count > 0:
                    pos_c = None
                    neg_c = None
                    if self._detector._positive_centroid is not None:
                        pos_c = np.asarray(self._detector._positive_centroid, dtype=np.float64).tobytes()
                    if self._detector._negative_centroid is not None:
                        neg_c = np.asarray(self._detector._negative_centroid, dtype=np.float64).tobytes()
                    await conn.execute(sqlite_upsert(FeedbackCentroidModel).values(
                        id="default",
                        positive_centroid=pos_c,
                        negative_centroid=neg_c,
                        last_computed_at=datetime.fromtimestamp(
                            self._detector._last_computed_at) if self._detector._last_computed_at else None,
                        example_count=self._detector._example_count,
                    ).on_conflict_do_update(
                        index_elements=["id"],
                        set_={
                            "positive_centroid": sqlite_upsert(FeedbackCentroidModel).excluded.positive_centroid,
                            "negative_centroid": sqlite_upsert(FeedbackCentroidModel).excluded.negative_centroid,
                            "last_computed_at": sqlite_upsert(FeedbackCentroidModel).excluded.last_computed_at,
                            "example_count": sqlite_upsert(FeedbackCentroidModel).excluded.example_count,
                        },
                    ))

    def embedding_info(self):
        """Which embedding backend context vectors are actually coming from.

        Distinct from "is the model tag installed in Ollama" (Kitty's
        ``EmbeddingModelStatus``) — this reflects whether ``EmbeddingProvider``
        has actually succeeded against it at least once. ``untried`` means no
        ``decide``/``record_outcome``/``record_annotation`` call carrying a
        ``context`` has fired yet.
        """
        available = self._embedder._ollama_available
        if available is True:
            backend = "ollama"
        elif available is False:
            backend = "hashing"
        else:
            backend = "untried"
        return {
            "backend": backend,
            "model": self._embedder.ollama_model,
            "url": self._embedder.ollama_url,
            "failed_decodes": self._embed_decode_failures,
        }

    def get_state(self):
        domains = self.list_domains()
        return {
            "embedding": self.embedding_info(),
            "sessions": len(self._sessions),
            "warm_ready": self._warm_ready,
            "action_history_len": len(self._action_history),
            "novelty_history_len": len(self._novelty_history),
            "schism_state": self._ensemble.schism_state.value,
            "preference_centroids_ready": self._detector.centroids_ready,
            "preference_example_count": self._detector._example_count,
            "bleed_temperature": self._bleed.default_temperature,
            "ttl_enabled": self._ttl.auto_enabled,
            "ensemble_diversity_mode": self._ensemble.ensemble_diversity_mode,
            "plateau_risk_score": self._ensemble.plateau_risk_score,
            "plateau_risk_components": self._ensemble.plateau_risk_components,
            "ensemble_weights": {
                "ig_weight_min": self._ensemble.ig_weight_min,
                "ig_weight_max": self._ensemble.ig_weight_max,
                "pc_weight": self._ensemble.pc_weight,
            },
            "novelty_lambda": self._novelty.get_lambda_for_mode("thought_partner"),
            "nudge_active": self._nudge.active,
            "domain_count": len(domains),
            "domains": domains,
            "feature_utilization": round(float(self._hasher.utilization), 3),
            "feature_collision_rate": round(float(self._hasher.collision_rate), 3),
            "discovered_primitives_count": len(self._primitive_discoverer.get_all_primitives()),
        }

    def get_metrics(self, time_range=None, domain=None, **kw):
        days_map = {"7d": 7, "14d": 14, "30d": 30}
        range_days = days_map.get(time_range, None)
        cutoff = None
        if range_days is not None:
            cutoff = time.time() - (range_days * 86400)

        edges = []
        for bucket_edges in self._edge_index.values():
            for e in bucket_edges:
                if domain and e.domain_id != domain:
                    continue
                edges.append(e)

        total_actions = len(self._action_history)
        override_rate = 0.0
        if total_actions > 0:
            positive_outcomes = sum(1 for e in edges if e.confidence > 0.6)
            override_rate = round(1.0 - (positive_outcomes / max(total_actions, 1)), 3)

        confidences = [e.confidence for e in edges if e.confidence is not None]
        conf_dist = {"0.0-0.3": 0, "0.3-0.6": 0, "0.6-0.8": 0, "0.8-1.0": 0}
        for c in confidences:
            if c < 0.3: conf_dist["0.0-0.3"] += 1
            elif c < 0.6: conf_dist["0.3-0.6"] += 1
            elif c < 0.8: conf_dist["0.6-0.8"] += 1
            else: conf_dist["0.8-1.0"] += 1

        domain_usage = {}
        for e in edges:
            did = e.domain_id or "unknown"
            domain_usage[did] = domain_usage.get(did, 0) + 1

        sorted_by_override = sorted(edges, key=lambda e: e.override_rate or 0, reverse=True)[:10]

        all_last_hints = []
        total_wildcard = 0
        total_uncertainty_slot = 0
        paused_sessions = 0
        for s in self._sessions.values():
            all_last_hints.extend(s.last_hints)
            total_wildcard += s.wildcard_count
            total_uncertainty_slot += s.uncertainty_slot_count
            if s.suggestions_paused:
                paused_sessions += 1
        pause_frequency = round(paused_sessions / len(self._sessions), 3) if self._sessions else 0.0

        annotation_counts = {}
        for ann in self._annotations_cache:
            if cutoff:
                try:
                    ts = time.mktime(time.strptime(ann["timestamp"], "%Y-%m-%dT%H:%M:%SZ"))
                    if ts < cutoff:
                        continue
                except (ValueError, OSError):
                    pass
            atype = ann.get("annotation_type", "unknown")
            annotation_counts[atype] = annotation_counts.get(atype, 0) + 1

        return {
            "metrics": {
                "total_actions_logged": total_actions,
                "total_edges_in_memory": len(edges),
                "override_rate": override_rate,
                "path_success_rate": round(
                    sum(1 for e in edges if e.confidence >= 0.6) / max(len(edges), 1), 3),
                "confidence_distribution": conf_dist,
                "annotation_counts": annotation_counts,
                "novelty_distribution": {
                    # Averaged over the actual edges' own context embeddings
                    # (a real read of current novelty pressure), not a fixed
                    # zero vector — the zero vector hashes to the same
                    # count-min-sketch buckets every time, so it reported a
                    # constant value regardless of the graph's real state.
                    # Falls back to the old zero-vector reading only when no
                    # edge has captured a context embedding at all yet.
                    "current_score": round(
                        float(np.mean([
                            self._novelty.current_score(e.embedding)
                            for e in edges if e.embedding is not None
                        ])) if any(e.embedding is not None for e in edges) else
                        float(self._novelty.current_score(np.zeros(self.config["embedding_dim"], dtype=np.float32))),
                        3),
                },
                "domain_usage": domain_usage,
                "pause_frequency": pause_frequency,
                "top_overridden_edges": [
                    {"id": e.id, "primitive": e.semantic_primitive,
                     "override_rate": e.override_rate or 0.0}
                    for e in sorted_by_override
                ],
                "time_range": time_range or "all",
                "domain_filter": domain,
                "discovered_primitives_count": len(self._primitive_discoverer.get_all_primitives()),
                "exploration_health": {
                    "ig_pc_hint_ratio": round(
                        sum(1 for h in all_last_hints
                            if getattr(h, "source_model", "standard") in ("ig", "pc", "wildcard", "uncertain")
                        ) / max(len(all_last_hints), 1), 3) if all_last_hints else 0.0,
                    "action_entropy_50w": round(self._compute_action_entropy(50), 3),
                    "unique_primitives_active": len(set(
                        e.semantic_primitive for bucket_edges in self._edge_index.values()
                        for e in bucket_edges)),
                    "wildcard_slot_used": total_wildcard,
                    "uncertainty_slot_used": total_uncertainty_slot,
                    "user_exploration_score": round(self._novelty.user_exploration_score, 3),
                },
            }
        }

    def list_edges(self, domain=None, primitive=None, confidence_min=None,
                   confidence_max=None, tier=None, status=None,
                   sort="confidence", order="desc", page=1, per_page=20, **kw):
        edges = []
        for bucket_edges in self._edge_index.values():
            for e in bucket_edges:
                if domain and e.domain_id != domain:
                    continue
                if primitive and e.semantic_primitive != primitive:
                    continue
                if confidence_min is not None and (e.confidence or 0) < confidence_min:
                    continue
                if confidence_max is not None and (e.confidence or 0) > confidence_max:
                    continue
                if tier and e.tier != tier:
                    continue
                if status and e.status.value != status:
                    continue
                edges.append({
                    "id": e.id,
                    "semantic_primitive": e.semantic_primitive,
                    "domain_id": e.domain_id,
                    "confidence": float(e.confidence or 0.5),
                    "status": e.status.value,
                    "tier": e.tier,
                    "frequency": e.frequency or 0,
                    "override_rate": e.override_rate or 0.0,
                    "last_accessed": e.last_accessed,
                    "created_at": e.created_at,
                    "ttl": None,
                    "tags": e.tags or [],
                    "domain_tags": e.domain_tags or [],
                })

        sort_key_map = {
            "confidence": lambda x: x["confidence"],
            "frequency": lambda x: x["frequency"],
            "last_accessed": lambda x: x["last_accessed"] or "",
            "override_rate": lambda x: x["override_rate"],
        }
        key_fn = sort_key_map.get(sort, lambda x: x["confidence"])
        reverse = order.lower() == "desc"
        edges.sort(key=key_fn, reverse=reverse)

        total = len(edges)
        start = (page - 1) * per_page
        end = start + per_page
        pages = max(1, (total + per_page - 1) // per_page)

        return {"edges": edges[start:end], "total": total, "page": page, "pages": pages}

    def get_edge(self, edge_id):
        for bucket_edges in self._edge_index.values():
            for edge in bucket_edges:
                if edge.id == edge_id:
                    return edge
        return None

    async def update_edge(self, edge_id, updates):
        edge = self.get_edge(edge_id)
        if edge is None:
            return False
        if "confidence" in updates:
            edge.confidence = float(updates["confidence"])
        if "semantic_primitive" in updates:
            edge.semantic_primitive = str(updates["semantic_primitive"])
        if "domain" in updates or "domain_id" in updates:
            edge.domain_id = str(updates.get("domain", updates.get("domain_id", edge.domain_id)))
        if "status" in updates:
            edge.status = EdgeStatus(updates["status"])
        if "ttl" in updates and updates["ttl"] is None:
            self._ttl.clear_ttl(edge_id)
            self._ttl_dirty.add(edge_id)
        if "tags" in updates:
            edge.tags = list(updates["tags"])
        if "domain_tags" in updates:
            edge.domain_tags = list(updates["domain_tags"])
        # Persist — a memory-only edit silently reverts on the next process
        # restart now that edges are actually stored.
        if self._engine:
            async with self._engine.begin() as conn:
                await self._persist_edge(conn, edge, created=False)
                await self._sync_ttl_store(conn)
        return True

    async def delete_edge(self, edge_id):
        for bucket, edges in list(self._edge_index.items()):
            for i, edge in enumerate(edges):
                if edge.id == edge_id:
                    edge.tier = "cold"
                    self._edge_index[bucket].pop(i)
                    if not self._edge_index[bucket]:
                        del self._edge_index[bucket]
                    if self._engine:
                        async with self._engine.begin() as conn:
                            await conn.execute(
                                sa.delete(EdgeModel).where(EdgeModel.id == edge_id))
                    return True
        # Not found in the warmed in-memory index — it may still exist as a
        # DB-only row (never warmed, or a stale/typo'd id). Check the actual
        # delete's rowcount instead of unconditionally reporting success, so
        # callers (the sidecar's DELETE /edges/{id}) can distinguish a real
        # delete from a no-op.
        if self._engine:
            async with self._engine.begin() as conn:
                result = await conn.execute(
                    sa.delete(EdgeModel).where(EdgeModel.id == edge_id))
                return result.rowcount > 0
        return False

    def list_annotations(self, annotation_type=None, domain=None, time_range=None,
                         detection_method=None, page=1, per_page=20, **kw):
        days_map = {"7d": 7, "14d": 14, "30d": 30}
        range_days = days_map.get(time_range, None)
        cutoff = None
        if range_days is not None:
            cutoff = time.time() - (range_days * 86400)

        results = []
        for ann in self._annotations_cache:
            if annotation_type and ann["annotation_type"] != annotation_type:
                continue
            if detection_method and ann["detection_method"] != detection_method:
                continue
            if cutoff:
                try:
                    ts = time.mktime(time.strptime(ann["timestamp"], "%Y-%m-%dT%H:%M:%SZ"))
                    if ts < cutoff:
                        continue
                except (ValueError, OSError):
                    pass
            results.append(ann)

        total = len(results)
        start = (page - 1) * per_page
        end = start + per_page
        pages = max(1, (total + per_page - 1) // per_page)
        return {"annotations": results[start:end], "total": total, "page": page, "pages": pages}

    def list_domains(self):
        self._rebuild_domain_cache()
        return list(self._domains_cache.values())

    def update_domain(self, domain_id, updates):
        self._rebuild_domain_cache()
        if domain_id not in self._domains_cache:
            return False
        domain = self._domains_cache[domain_id]
        if "name" in updates:
            domain["name"] = str(updates["name"])
        if "dpp_diversity_weight" in updates:
            domain["dpp_diversity_weight"] = float(updates["dpp_diversity_weight"])
        if "novelty_lambda" in updates:
            domain["novelty_lambda"] = float(updates["novelty_lambda"])
        if "locked" in updates:
            domain["locked"] = bool(updates["locked"])
        return True

    def _rebuild_domain_cache(self):
        domains = {}
        for bucket_edges in self._edge_index.values():
            for edge in bucket_edges:
                did = edge.domain_id or "default"
                if did not in domains:
                    domains[did] = {
                        "id": did,
                        "name": did,
                        "source": "auto_named",
                        "dpp_diversity_weight": 1.0,
                        "novelty_lambda": 0.5,
                        "revision_rate": 0.0,
                        "acceptance_rate": 0.0,
                        "sessions": 0,
                        "edge_count": 0,
                        "override_rate": 0.0,
                        "last_inferred": None,
                        "locked": False,
                    }
                domains[did]["edge_count"] += 1
        self._domains_cache = domains

    async def reset_domain(self, domain_id, mode="soft"):
        if mode == "hard":
            return await self._hard_reset_domain(domain_id)
        return await self._soft_reset_domain(domain_id)

    async def _soft_reset_domain(self, domain_id):
        touched = []
        for bucket_edges in self._edge_index.values():
            for edge in bucket_edges:
                if edge.domain_id == domain_id:
                    edge.confidence = max(0.1, (edge.confidence or 0.5) * 0.5)
                    touched.append(edge)
        for i in range(4):
            for aid in range(self._ensemble.n_actions):
                self._ensemble.models[i].A_inv[aid] = (
                    self._ensemble.models[i].A_inv[aid] * 0.5 +
                    np.eye(self._ensemble.d_features) * 0.5)
        if self._engine and touched:
            async with self._engine.begin() as conn:
                for bucket in set(self._bucketer.get_bucket(e.semantic_primitive) for e in touched):
                    await self._persist_ensemble_state(conn, bucket)
                for edge in touched:
                    await self._persist_edge(conn, edge, created=False)
        return True

    async def _hard_reset_domain(self, domain_id):
        touched = []
        for bucket_edges in self._edge_index.values():
            for edge in bucket_edges:
                if edge.domain_id == domain_id:
                    edge.confidence = 0.1
                    edge.tier = "cold"
                    touched.append(edge)
        for i in range(6):
            if i == self._ensemble.pc_model_index:
                continue
            for aid in range(self._ensemble.n_actions):
                self._ensemble.models[i].A_inv[aid] = np.eye(
                    self._ensemble.d_features, dtype=np.float64)
                self._ensemble.models[i].b[aid] = np.zeros(
                    self._ensemble.d_features, dtype=np.float64)
        if self._engine and touched:
            async with self._engine.begin() as conn:
                for bucket in set(self._bucketer.get_bucket(e.semantic_primitive) for e in touched):
                    await self._persist_ensemble_state(conn, bucket)
                for edge in touched:
                    await self._persist_edge(conn, edge, created=False)
        return True

    def export_graph(self, include_annotations=False, include_ensemble_state=False,
                     domain=None, **kw):
        edges_out = []
        for bucket_edges in self._edge_index.values():
            for edge in bucket_edges:
                if domain and edge.domain_id != domain:
                    continue
                edges_out.append({
                    "id": edge.id,
                    "semantic_primitive": edge.semantic_primitive,
                    "domain_id": edge.domain_id,
                    "confidence": float(edge.confidence or 0.5),
                    "status": edge.status.value,
                    "tier": edge.tier,
                    "frequency": edge.frequency or 0,
                    "override_rate": edge.override_rate or 0.0,
                    "tags": edge.tags or [],
                    "domain_tags": edge.domain_tags or [],
                    "co_selected_with": edge.co_selected_with or [],
                    "last_accessed": edge.last_accessed,
                    "created_at": edge.created_at,
                    "embedding": edge.embedding.tolist() if edge.embedding is not None else None,
                })
        result = {
            "version": "0.1.0",
            "exported_at": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "edges": edges_out,
            "domains": self.list_domains(),
        }
        if include_annotations:
            result["annotations"] = self.list_annotations()["annotations"]
        if include_ensemble_state:
            result["ensemble_state"] = {
                "schism_state": self._ensemble.schism_state.value,
                "plateau_risk_score": self._ensemble.plateau_risk_score,
                "plateau_risk_components": self._ensemble.plateau_risk_components,
            }
        return result

    async def import_graph(self, data, mode="merge", target_domain=None):
        edges = data.get("edges", [])
        if mode == "replace_all":
            self._edge_index.clear()
            self._tiered = TieredCache(self.config, self._vec_index, self._bucketer)
            # The in-memory index above is cleared either way, but without
            # also clearing the DB, every old row was still sitting there —
            # invisible until the next restart's `_warm_data` re-loaded them
            # straight back into `_edge_index`, silently undoing the
            # "replace all" the caller asked for. Runs even when `edges` is
            # empty (a "clear everything, import nothing" call) — this used
            # to bail out before even reaching this block in that case.
            if self._engine:
                async with self._engine.begin() as conn:
                    await conn.execute(sa.delete(EdgeModel))
                    await conn.execute(sa.delete(NodeModel))
        if not edges:
            return True

        existing_ids = set()
        for bucket_edges in self._edge_index.values():
            for e in bucket_edges:
                existing_ids.add(e.id)

        new_edges = []
        merged_edges = []
        for edge_data in edges:
            eid = edge_data.get("id", str(uuid.uuid4()))
            if mode == "merge" and eid in existing_ids:
                existing = self.get_edge(eid)
                if existing and (edge_data.get("confidence", 0.5) > (existing.confidence or 0.5)):
                    existing.confidence = edge_data["confidence"]
                    merged_edges.append(existing)
            else:
                embedding = edge_data.get("embedding")
                edge = EdgeInfo(
                    id=eid,
                    semantic_primitive=edge_data.get("semantic_primitive", ""),
                    domain_id=edge_data.get("domain_id", ""),
                    domain=edge_data.get("domain_id", ""),
                    confidence=edge_data.get("confidence", 0.5),
                    status=EdgeStatus(edge_data.get("status", "provisional")),
                    tier=edge_data.get("tier", "hot"),
                    frequency=edge_data.get("frequency", 0),
                    override_rate=edge_data.get("override_rate", 0.0),
                    tags=edge_data.get("tags", []),
                    domain_tags=edge_data.get("domain_tags", []),
                    co_selected_with=edge_data.get("co_selected_with", []),
                    embedding=np.asarray(embedding, dtype=np.float32) if embedding is not None else None,
                )
                bucket = self._bucketer.get_bucket(edge.semantic_primitive)
                self._edge_index.setdefault(bucket, []).append(edge)
                new_edges.append(edge)

        if mode == "replace_domain" and target_domain:
            for bucket_edges in self._edge_index.values():
                for edge in bucket_edges:
                    if edge.domain_id == target_domain:
                        edge.tier = "cold"

        # Persist imported edges and repopulate TieredCache (row 14:
        # replace_all built a fresh empty TieredCache without populating it).
        if self._engine and new_edges:
            async with self._engine.begin() as conn:
                for edge in new_edges:
                    await self._persist_edge(conn, edge, created=True)
            for edge in new_edges:
                self._tiered.add_hot(edge)

        # A merge-mode confidence bump on an *existing* edge used to be
        # memory-only, reverting to its pre-import value on the next restart.
        if self._engine and merged_edges:
            async with self._engine.begin() as conn:
                for edge in merged_edges:
                    await self._persist_edge(conn, edge, created=False)

        return True

    def query_attribution(self, attribution_id):
        logged = self._attribution_log.get(attribution_id)
        edge = None
        if logged and logged.get("edge_id"):
            edge = self.get_edge(logged["edge_id"])
        if edge is None:
            edge = self.get_edge(attribution_id)
        if edge is None:
            return None
        bucket = self._bucketer.get_bucket(edge.semantic_primitive)
        dummy_ctx = np.zeros(self._hasher.n_buckets, dtype=np.float64)
        _, raw_samples = self._ensemble.sample(bucket, dummy_ctx)
        agreement = self._ensemble.agreement(bucket, dummy_ctx)

        edges_for_domain = [e for e in self._get_all_edges() if e.domain_id == edge.domain_id]

        domain_stats = {}
        for e in edges_for_domain:
            did = e.domain_id or "default"
            if did not in domain_stats:
                domain_stats[did] = {"confidences": []}
            domain_stats[did]["confidences"].append(e.confidence or 0.5)

        pc_model = self._ensemble.models[self._ensemble.pc_model_index]
        top_ids = [e.id for e in edges_for_domain[:5]]
        pc_score = pc_model.score(edge.id, dummy_ctx, top_ids, {
            did: {"avg_confidence": float(np.mean(s["confidences"])) if s["confidences"] else 0.5}
            for did, s in domain_stats.items()
        })

        return {
            "edge_id": edge.id,
            "semantic_primitive": edge.semantic_primitive,
            "domain": edge.domain_id,
            "confidence": float(edge.confidence or 0.5),
            "tier": edge.tier,
            "thompson_predictions": agreement["predictions"],
            "ensemble_mean": agreement["mean"],
            "ensemble_std": agreement["std"],
            "ensemble_disagree": agreement["disagree"],
            "ig_model_score": agreement["ig_model_score"],
            "pc_model_score": pc_score,
            "dpp_rank": None,
            "alternatives_considered": [e.semantic_primitive for e in edges_for_domain[:5]],
            "raw_samples": [float(s) for s in raw_samples],
            "rationale": logged.get("rationale") if logged else None,
            "source_model": logged.get("source_model", "standard") if logged else "standard",
        }

    def toggle_suggestions(self, session_id, paused):
        state = self._sessions.get(session_id)
        if state:
            state.suggestions_paused = paused
            if state.selector:
                state.selector.set_context(
                    domain_hint=state.domain_hint,
                    suggestions_paused=paused,
                    mode=state.mode,
                )
            return True
        return False

    def get_schism_alert(self):
        if self._ensemble.schism_state == SchismState.NONE:
            return None
        sd = self._ensemble.schism_data
        if sd is None:
            return None
        detected_at = None
        if self._ensemble.schism_detected_at is not None:
            detected_at = time.strftime(
                "%Y-%m-%dT%H:%M:%SZ", time.gmtime(self._ensemble.schism_detected_at)
            )
        return {
            "state": self._ensemble.schism_state.value,
            "faction_a": sd["fa"],
            "faction_b": sd["fb"],
            "within_a": sd["wa"],
            "within_b": sd["wb"],
            "between": sd["bt"],
            "faction_a_models": len(sd["fa"]),
            "faction_b_models": len(sd["fb"]),
            "detected_at": detected_at,
        }

    def resolve_schism(self, keep_faction):
        if self._ensemble.schism_state == SchismState.NONE:
            return False
        if self._ensemble.schism_state == SchismState.DETECTED:
            self._ensemble.schism_state = SchismState.REVIEWING
        try:
            self._ensemble.resolve(keep_faction)
            return True
        except ValueError:
            return False

    async def update_ensemble_weights(self, ig_weight_min=None, ig_weight_max=None, pc_weight=None):
        new_min = ig_weight_min if ig_weight_min is not None else self._ensemble.ig_weight_min
        new_max = ig_weight_max if ig_weight_max is not None else self._ensemble.ig_weight_max
        new_pc = pc_weight if pc_weight is not None else self._ensemble.pc_weight

        for name, val in (("ig_weight_min", new_min), ("ig_weight_max", new_max), ("pc_weight", new_pc)):
            if not (0.0 <= val <= 1.0):
                return {"error": f"{name} must be between 0 and 1, got {val}"}
        if new_min > new_max:
            return {"error": f"ig_weight_min ({new_min}) must be <= ig_weight_max ({new_max})"}
        if new_max + new_pc > 0.9:
            return {"error": f"ig_weight_max + pc_weight must be <= 0.9 "
                              f"(got {new_max + new_pc:.3f}) to leave room for standard models"}

        self._ensemble.ig_weight_min = new_min
        self.config["ensemble"]["ig_weight_min"] = new_min
        self._ensemble.ig_weight_max = new_max
        self.config["ensemble"]["ig_weight_max"] = new_max
        self._ensemble.pc_weight = new_pc
        self.config["ensemble"]["pc_weight"] = new_pc
        # Persist to survive restart (row 13).
        if self._engine:
            async with self._engine.begin() as conn:
                await conn.execute(sqlite_upsert(AppSettingsModel).values(
                    key="ensemble_weights",
                    value={
                        "ig_weight_min": new_min,
                        "ig_weight_max": new_max,
                        "pc_weight": new_pc,
                    },
                ).on_conflict_do_update(
                    index_elements=["key"],
                    set_={"value": sqlite_upsert(AppSettingsModel).excluded.value},
                ))
        return {
            "ig_weight_min": self._ensemble.ig_weight_min,
            "ig_weight_max": self._ensemble.ig_weight_max,
            "pc_weight": self._ensemble.pc_weight,
        }

    def submit_micro_annotation(self, session_id, atype, action_id):
        state = self._sessions.get(session_id)
        if state and state.suggestions_paused and atype in ("micro_positive", "micro_negative"):
            return True
        import asyncio
        try:
            loop = asyncio.get_running_loop()
            loop.create_task(self.record_annotation(session_id, {
                "type": atype,
                "edge_id": action_id,
                "action_id": action_id,
                "context_embedding": np.zeros(self.config["embedding_dim"], dtype=np.float32),
            }))
        except RuntimeError:
            pass
        return True

    def dismiss_nudge(self):
        self._nudge.dismiss()

    def health_check(self):
        issues = []
        ch = self.config["health"]
        utilization = float(self._hasher.utilization)
        collision = float(self._hasher.collision_rate)
        if collision > ch.get("collision_rate_warning", 0.15):
            issues.append({
                "severity": "warning", "component": "features",
                "message": f"Feature collision rate {round(collision, 3)} exceeds threshold",
                "details": {"collision_rate": round(collision, 3), "threshold": ch["collision_rate_warning"]},
            })
        if utilization > 0.80:
            issues.append({
                "severity": "warning", "component": "features",
                "message": f"Feature utilization {round(utilization, 3)} is high",
                "details": {"utilization": round(utilization, 3)},
            })

        total_edges = sum(len(v) for v in self._edge_index.values())
        if total_edges == 0:
            issues.append({
                "severity": "info", "component": "graph",
                "message": "No edges in graph - cold start",
                "details": {},
            })

        if self._ensemble.schism_state != SchismState.NONE:
            issues.append({
                "severity": "warning", "component": "ensemble",
                "message": f"Schism state: {self._ensemble.schism_state.value}",
                "details": {"state": self._ensemble.schism_state.value},
            })

        novelty_vals = self._novelty_history[-100:] if self._novelty_history else []
        if novelty_vals and len(novelty_vals) >= 50:
            stale_count = sum(1 for v in novelty_vals if v < ch.get("stale_novelty_threshold", 0.05))
            stale_pct = stale_count / len(novelty_vals)
            if stale_pct > ch.get("stale_novelty_pct", 0.90):
                issues.append({
                    "severity": "warning", "component": "novelty",
                    "message": f"Novelty stale: {round(stale_pct, 3)} scores below threshold",
                    "details": {"stale_percentage": round(stale_pct, 3), "threshold": ch["stale_novelty_pct"]},
                })

        if not self._detector.centroids_ready and self._detector._example_count > 0:
            issues.append({
                "severity": "info", "component": "preferences",
                "message": "Preference centroids not yet computed",
                "details": {"example_count": self._detector._example_count},
            })

        return issues

    def _clamp_micro_reward(self, state, reward_weight):
        """Caps cumulative micro_positive/micro_negative reward per session
        (config `telemetry.per_session_cap`). Without this, a single heavy
        session generating many implicit signals (e.g. repeated Copy clicks
        while pulling material for a research project) can ratchet a
        topic's confidence far past what any one signal should be worth;
        this makes repeated micro-signals saturate instead of ratcheting
        without limit."""
        if state is None:
            return reward_weight
        cap = self.config["telemetry"].get("per_session_cap", 0.25)
        remaining = max(0.0, cap - state.micro_reward_used)
        applied = max(-remaining, min(remaining, reward_weight))
        state.micro_reward_used += abs(applied)
        return applied

    def _get_domain(self, edge_id):
        # Passed to ParadigmChallengeModel as get_domain_fn, always called
        # with an *edge id* (see selector._compute_hints / query_attribution)
        # — it must resolve to that edge's real domain_id, not echo the
        # edge id back verbatim (which used to make the id/domain keys in
        # ParadigmChallengeModel.score() never match domain_stats' real
        # domain-id keys, permanently zeroing the confidence_gap and
        # novelty_persistence signals).
        edge = self.get_edge(edge_id)
        if edge is not None:
            return edge.domain_id or edge.domain or ""
        return edge_id if edge_id else ""

    def _compute_action_entropy(self, window=50):
        recent = self._action_history[-window:]
        if len(recent) < 10:
            return 0.0
        counts = Counter(recent)
        t = len(recent)
        ent = -sum((c / t) * np.log(max(c / t, 1e-10)) for c in counts.values())
        return ent / np.log(max(len(counts), 2))

    def generate_session_reflection(self, session_id):
        edges = self._get_all_edges()
        top_domains = Counter(
            e.domain_id for e in edges if e.domain_id).most_common(3)
        deh = self.get_metrics()["metrics"].get("exploration_health", {})
        acceptance = round(
            self._ensemble.plateau_risk_score, 3) if hasattr(self._ensemble, "plateau_risk_score") else 0.0
        unchosen_novel = 0
        for e in edges:
            emb = e.embedding if e.embedding is not None else np.zeros(
                self.config["embedding_dim"], dtype=np.float32)
            nv = self._novelty.current_score(np.asarray(emb, dtype=np.float32).ravel())
            if nv > 0.4 and (e.frequency or 0) == 0:
                unchosen_novel += 1
        top_domain_str = top_domains[0][0] if top_domains else "new territory"
        return {
            "session_id": session_id,
            "top_domains": top_domains,
            "acceptance_score": acceptance,
            "unchosen_novel_edges": unchosen_novel,
            "reflection": (
                f"Score {acceptance:.3f}, mostly in {top_domain_str}. "
                f"{unchosen_novel} untested approaches available."
            ),
            "has_untested": unchosen_novel > 0,
            "exploration_health": deh,
        }

    def _get_edge(self, edge_id):
        return self.get_edge(edge_id)

    def _get_edges(self, action_ids):
        edges = []
        for aid in action_ids:
            bucket = self._bucketer.get_bucket(str(aid))
            edges.extend(self._tiered.get_by_bucket(bucket))
        return edges

    def _get_all_edges(self):
        edges = []
        for bucket_edges in self._edge_index.values():
            edges.extend(bucket_edges)
        return edges

    def _get_novelty_values(self):
        return self._novelty_history

    def _deep_update(self, d, key, value):
        keys = key.split(".")
        for k in keys[:-1]:
            d = d.setdefault(k, {})
        d[keys[-1]] = value

    def list_primitives(self):
        return self._primitive_discoverer.get_all_primitives()

    def get_primitives_info(self):
        return {
            name: self._primitive_discoverer.get_primitive_info(name)
            for name in self._primitive_discoverer.get_all_primitives()
        }

    def infer_domain(self, context_embedding, available_actions=None):
        edges = []
        if available_actions:
            edges = self._get_edges(available_actions)
        return self._domain_discovery.infer_domain(
            context_embedding, available_actions or [], edges)

    def set_domain(self, domain_id, name, source="user_named"):
        from .types import DomainSource
        src = DomainSource(source) if isinstance(source, str) else source
        return self._domain_discovery.add_domain(domain_id, name, source=src)

    def get_graph_health(self):
        return self._health_checker.get_graph_health()

    def embed_context(self, text):
        """Turn free-text context into an embedding (Ollama, falling back to
        a deterministic hashing vectorizer). Synchronous and potentially
        network-bound (Ollama) — callers on an async path (MCP tools,
        sidecar routes) must run this via e.g. `asyncio.to_thread` rather
        than awaiting it directly, and must not call it from inside
        `decide()` itself, which stays sync/zero-I/O."""
        return self._embedder.embed(text)
