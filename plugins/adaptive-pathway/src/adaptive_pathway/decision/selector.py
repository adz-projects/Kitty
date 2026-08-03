import uuid
import numpy as np
from ..types import DecisionResult, InSessionStatus, Hint, PlateauRisk
from .diversity import build_dpp_kernel, dpp_sample
from .blending import find_blendable_pairs, blend_hints

class ActionSelector:
    def __init__(self, ensemble, in_session_bandit, novelty, config,
                 get_domain_fn, get_edges_fn, bucketer, hasher,
                 action_history=None, novelty_history=None,
                 bleed=None, ttl=None):
        self._ensemble = ensemble
        self._in_session = in_session_bandit
        self._novelty = novelty
        self._config = config
        self._get_domain = get_domain_fn
        self._get_edges = get_edges_fn
        self._bucketer = bucketer
        self._hasher = hasher
        self._bleed = bleed
        self._ttl = ttl
        self._action_history = action_history if action_history is not None else []
        self._novelty_history = novelty_history if novelty_history is not None else []
        self._call_count = 0
        self._nudge = None
        self._domain_hint = None
        self._suggestions_paused = False
        self._mode = "thought_partner"
        self._last_turn_time = 0.0
        self._consecutive_rapid = 0
        self._in_flow = False
        self._dpp_config = config["dpp"]
        self._flow_config = config["flow"]
        self._wildcard_config = config.get("wildcard", {})
        self._wildcard_count = 0
        self._novelty_lambda_boost = 0.0
        self._pc_label_threshold = config.get("paradigm_challenge", {}).get("label_threshold", 0.5)
        self._exploration_slot_config = config.get("exploration_slot", {})

    def set_nudge(self, nudge):
        self._nudge = nudge

    def set_novelty_lambda_boost(self, boost):
        self._novelty_lambda_boost = boost

    def set_context(self, domain_hint=None, suggestions_paused=False, mode="thought_partner"):
        self._domain_hint = domain_hint
        self._suggestions_paused = suggestions_paused
        self._mode = mode

    def select(self, session_id, context_embedding, available_actions,
               domain_id=None, suggestions_paused=False):
        self._call_count += 1
        self.set_context(domain_id, suggestions_paused, self._mode)

        now = np.float64(0.0)
        try:
            import time
            now = time.time()
        except Exception:
            pass
        time_since_last = now - self._last_turn_time if self._last_turn_time > 0 else float("inf")
        self._last_turn_time = now
        if time_since_last < self._flow_config["max_pause_seconds"]:
            self._consecutive_rapid += 1
        else:
            self._consecutive_rapid = 0
        self._in_flow = self._consecutive_rapid >= self._flow_config["consecutive_turns_threshold"]

        ctx_raw = np.asarray(context_embedding, dtype=np.float32).ravel()
        ctx_features = np.asarray(self._hasher.hash_embedding(ctx_raw), dtype=np.float64)

        if self._suggestions_paused and not self._in_flow:
            hints = []
        else:
            hints = self._compute_hints(session_id, ctx_features, ctx_raw, available_actions, domain_id)

        novelty_score = float(self._novelty.current_score(ctx_raw))

        confidence = np.mean([h.confidence for h in hints]) if hints else 0.5
        attribution_ids = [h.attribution_id for h in hints]
        novelty_val = novelty_score

        plateau_risk = None
        if self._call_count % self._config["plateau_risk"]["check_interval"] == 0:
            plateau_risk = self._ensemble.evaluate_plateau_risk(
                self._action_history, self._novelty_history)

        nudge_active = None
        if self._nudge and self._nudge.active:
            nudge_active = self._nudge.status()

        nudge_offered = False
        if self._nudge and self._call_count % self._config["curiosity_nudge"]["check_interval"] == 0:
            detection = self._nudge.detect_plateau(self._action_history)
            if detection.is_plateau:
                reason = f"Low entropy ({detection.entropy}), high concentration ({detection.top3_concentration})"
                offer = self._nudge.offer(reason, self._mode)
                nudge_offered = offer is not None

        in_session_status = InSessionStatus(
            mix_weight=round(self._in_session.mix_weight, 3),
            call_count=self._in_session.call_count,
            max_weight=self._in_session.max_weight,
            buffer_size=self._in_session.buffer_size,
        )

        exploration_metrics = self._compute_exploration_metrics(hints)

        return DecisionResult(
            hints=hints,
            confidence=round(float(confidence), 3),
            novelty=round(float(novelty_val), 3),
            attribution_ids=attribution_ids,
            is_flow_state=self._in_flow,
            plateau_risk=plateau_risk,
            in_session=in_session_status,
            nudge_active=nudge_active,
            nudge_offered=nudge_offered,
            exploration_metrics=exploration_metrics,
        )

    def _compute_exploration_metrics(self, hints):
        wildcard = sum(1 for h in hints if getattr(h, "source_model", "standard") == "wildcard")
        uncertain = sum(1 for h in hints if getattr(h, "source_model", "standard") == "uncertain")
        ig_pc = sum(1 for h in hints if getattr(h, "source_model", "standard") in ("ig", "pc"))
        return {
            "wildcard_count": wildcard,
            "uncertainty_slot_count": uncertain,
            "ig_pc_count": ig_pc + uncertain,
            "standard_count": len(hints) - wildcard - uncertain - ig_pc,
            "user_exploration_score": round(self._novelty.user_exploration_score, 3),
        }

    def _compute_hints(self, session_id, ctx_features, ctx_raw, available_actions, domain_id):
        edges = self._get_edges(available_actions)
        if not edges:
            return []

        if self._ttl:
            edges = [e for e in edges if not self._ttl.is_expired(e.id)]

        novelty_lambda = self._novelty.get_lambda_for_mode(self._mode)
        if self._in_flow:
            novelty_lambda = max(
                novelty_lambda * self._flow_config["novelty_lambda_reduction"],
                self._flow_config.get("novelty_lambda_floor", 0.05))
        if self._nudge and self._nudge.active:
            novelty_lambda = self._nudge.apply(novelty_lambda)
        novelty_lambda = max(novelty_lambda, self._novelty.lambda_floor)
        novelty_lambda = min(novelty_lambda + self._novelty_lambda_boost, 1.0)

        query_domain = domain_id or self._domain_hint or ""
        domain_lambda = self._novelty.get_lambda_for_domain(query_domain) if query_domain else novelty_lambda

        scored = []
        # Multiple edges commonly hash to the same action bucket (bounded by
        # max_action_buckets); ctx_features is fixed for this call, so their
        # bucket-keyed ensemble draws (Thompson + IG) are identical samples
        # for the identical underlying (bucket, context) pair. Memoize the
        # base per bucket and add the per-edge paradigm-challenge term on
        # top — cuts Thompson-sampling calls (the dominant decide() cost)
        # roughly in proportion to the collision rate, with no change to
        # the sampled distribution. The PC term must be edge-aware or it is
        # constant across every edge sharing a bucket (see
        # BootstrapEnsemble.sample_edge_aware).
        domain_stats = self._compute_domain_stats(edges)
        pc_top_n = self._config["paradigm_challenge"]["top_n"]
        pc_referent_ids = [e.id for e in edges[:50]][:pc_top_n]
        # Referents are the first `top_n` candidate edges, already in hand —
        # resolve their domains/edges once instead of per scored edge
        # (id-lookups cost O(edges) each).
        pc_referent_edges = edges[:pc_top_n]
        pc_referent_domains = {e.domain_id or e.domain or "" for e in pc_referent_edges}
        pc_referents = (pc_referent_domains, pc_referent_edges)
        bucket_sample_cache = {}
        pc_scores = {}
        for edge in edges[:50]:
            bucket = self._bucketer.get_bucket(edge.semantic_primitive)
            if bucket not in bucket_sample_cache:
                base = self._ensemble.base_samples(bucket, ctx_features)
                in_session_score = self._in_session.sample(bucket, ctx_features)
                bucket_sample_cache[bucket] = (base, in_session_score)
            base, in_session_score = bucket_sample_cache[bucket]
            ensemble_score, raw_samples = self._ensemble.sample_edge_aware(
                edge.id, bucket, ctx_features, pc_referent_ids, domain_stats,
                base=base, referents=pc_referents)
            pc_scores[edge.id] = raw_samples[-1]
            combined = (1.0 - self._in_session.mix_weight) * ensemble_score + self._in_session.mix_weight * in_session_score
            novelty_bonus = self._novelty.bonus(ctx_raw, lambda_override=novelty_lambda)
            ubi = self._novelty.action_bonus(edge.semantic_primitive, lambda_override=novelty_lambda)
            domain_bonus = self._novelty.bonus(ctx_raw, lambda_override=domain_lambda) if domain_lambda != novelty_lambda else 0.0

            bleed_factor = 1.0
            if self._bleed and query_domain:
                bleed_factor = self._bleed.bleed_score(query_domain, edge.domain_id or edge.domain or "")
            edge_score = (float(combined) + float(novelty_bonus) + float(ubi) + float(domain_bonus)) * bleed_factor
            scored.append((edge_score, edge))

        scored.sort(key=lambda x: x[0], reverse=True)

        # Reserve one hint slot for the uncertainty-guaranteed pick below —
        # only when there's actually a pool of edges outside the top-K to
        # promote from; otherwise every edge is already in top_edges and
        # reserving would just shrink the hint list for no benefit.
        reserve_uncertainty_slot = (
            self._exploration_slot_config.get("enabled", True) and len(scored) > 1)
        effective_max_hints = self._dpp_config["max_hints"] - (1 if reserve_uncertainty_slot else 0)
        effective_max_hints = max(effective_max_hints, 1)

        top_k = min(effective_max_hints, len(scored))
        top_edges = [e for _, e in scored[:max(top_k + 5, min(20, len(scored)))]]
        top_scores = [s for s, _ in scored[:len(top_edges)]]

        if len(top_edges) >= 2:
            embeddings_array = [e.embedding if e.embedding is not None else np.zeros(self._config["embedding_dim"], dtype=np.float64) for e in top_edges]
            try:
                kernel = build_dpp_kernel(embeddings_array, top_scores,
                                        diversity_weight=self._dpp_config["default_diversity_weight"])
                selected_idx = dpp_sample(kernel, top_k, epsilon=self._dpp_config["epsilon"])
                top_edges = [top_edges[i] for i in selected_idx if i < len(top_edges)]
            except Exception:
                top_edges = top_edges[:top_k]
        else:
            top_edges = top_edges[:top_k]

        hints = []
        for edge in top_edges:
            rationale = self._build_rationale(edge, query_domain)
            source_model = "standard"
            if pc_scores.get(edge.id, 0.0) >= self._pc_label_threshold:
                source_model = "pc"
                rationale = f"{rationale}; challenges the current paradigm"
            text = f"{edge.semantic_primitive} — {rationale}"
            hint = Hint(
                text=text,
                confidence=round(float(edge.confidence), 3),
                primitive=edge.semantic_primitive,
                domain=edge.domain or edge.domain_id or "",
                attribution_id=str(uuid.uuid4()),
                edge_id=edge.id,
                rationale=rationale,
                source_model=source_model,
            )
            hints.append(hint)

        # EdgeInfo is a plain (unhashable) dataclass — `set(top_edges)`
        # raised TypeError whenever a lookup like this was reached (graphs
        # with fewer than dpp.max_hints edges, e.g. cold start / narrow
        # domains), so this is a real-world-reachable crash, not just a
        # test artifact. Shared by both the uncertainty slot and the
        # wildcard slot below so neither can double-suggest the same edge.
        top_edge_ids = {e.id for e in top_edges}

        if reserve_uncertainty_slot and len(hints) < self._dpp_config["max_hints"]:
            uncertain_candidates = [e for e in edges[:max(50, len(edges))]
                                     if e.id not in top_edge_ids]
            if uncertain_candidates:
                bucket_sigma_cache = {}
                best_edge, best_sigma = None, -1.0
                for e in uncertain_candidates:
                    b = self._bucketer.get_bucket(e.semantic_primitive)
                    if b not in bucket_sigma_cache:
                        bucket_sigma_cache[b] = self._ensemble.max_sigma(b, ctx_features)
                    sigma = bucket_sigma_cache[b]
                    if sigma > best_sigma:
                        best_sigma, best_edge = sigma, e
                if best_edge is not None:
                    label = self._exploration_slot_config.get("label", "least understood")
                    uc_rationale = f"🔍 {label}: we have the least data on this"
                    uc_hint = Hint(
                        text=f"🔍 {best_edge.semantic_primitive} — {uc_rationale}",
                        confidence=round(float(best_edge.confidence), 3),
                        primitive=best_edge.semantic_primitive,
                        domain=best_edge.domain or best_edge.domain_id or "",
                        attribution_id=str(uuid.uuid4()),
                        edge_id=best_edge.id,
                        rationale=uc_rationale,
                        source_model="uncertain",
                    )
                    hints.append(uc_hint)
                    top_edge_ids.add(best_edge.id)

        if self._wildcard_config.get("enabled", True) and len(hints) < self._dpp_config["max_hints"]:
            remaining = [e for e in edges[:max(50, len(edges))]
                         if e.id not in top_edge_ids]
            scored_wildcard = []
            for e in remaining:
                pc_s = self._ensemble.models[self._ensemble.pc_model_index].score_with_referents(
                    e.id, ctx_features, pc_referent_domains, pc_referent_edges, domain_stats)
                emb = e.embedding if e.embedding is not None else np.zeros(self._config["embedding_dim"], dtype=np.float32)
                nov = float(self._novelty.current_score(np.asarray(emb, dtype=np.float32).ravel()))
                scored_wildcard.append((pc_s * self._wildcard_config.get("pc_weight", 0.7) +
                                        nov * self._wildcard_config.get("novelty_weight", 0.3), e))
            scored_wildcard.sort(key=lambda x: x[0], reverse=True)
            max_slots = min(self._wildcard_config.get("max_slots", 1),
                          self._dpp_config["max_hints"] - len(hints))
            threshold = self._wildcard_config.get("min_score_threshold", 0.15)
            label = self._wildcard_config.get("label", "untested angle")
            for i in range(min(max_slots, len(scored_wildcard))):
                wc_score, wc_edge = scored_wildcard[i]
                if wc_score > threshold:
                    wc_rationale = f"💡 {label}: may fill a paradigm gap"
                    self._wildcard_count += 1
                    wc_hint = Hint(
                        text=f"💡 {wc_edge.semantic_primitive} — {wc_rationale}",
                        confidence=round(float(wc_edge.confidence), 3),
                        primitive=wc_edge.semantic_primitive,
                        domain=wc_edge.domain or wc_edge.domain_id or "",
                        attribution_id=str(uuid.uuid4()),
                        edge_id=wc_edge.id,
                        rationale=wc_rationale,
                        source_model="wildcard",
                    )
                    hints.append(wc_hint)

        ec = self._config["edge_blending"]
        if ec["enabled"]:
            blend_pairs = find_blendable_pairs(
                top_edges,
                min_confidence=ec["min_confidence"],
                require_shared_domain=ec["require_shared_domain"],
                max_blends=ec["max_blends_per_call"],
            )
            blended = blend_hints(blend_pairs)
            hints.extend(blended)

        self._novelty.visit(ctx_raw)
        for edge in top_edges:
            self._novelty.visit_action(edge.semantic_primitive)
        return hints

    def _build_rationale(self, edge, domain_id=""):
        parts = []
        if edge.frequency and edge.frequency > 10:
            parts.append(f"succeeded in {edge.frequency} contexts")
        if edge.confidence and edge.confidence > 0.7:
            parts.append(f"confidence {edge.confidence:.0%}")
        if edge.domain_id and domain_id and edge.domain_id == domain_id:
            parts.append("domain match")
        if not parts:
            parts.append(f"suggested (confidence {edge.confidence:.0%})")
        return "; ".join(parts)

    def _compute_domain_stats(self, edges):
        stats = {}
        for edge in edges:
            did = edge.domain_id or edge.domain or ""
            if did not in stats:
                stats[did] = {"confidences": [], "edges": []}
            stats[did]["confidences"].append(edge.confidence)
            stats[did]["edges"].append(edge)
        result = {}
        for did, s in stats.items():
            if s["edges"] and s["edges"][0].embedding is not None:
                embeddings = [e.embedding for e in s["edges"] if e.embedding is not None]
                if embeddings:
                    emb_stack = np.stack([np.asarray(e, dtype=np.float32).ravel() for e in embeddings])
                    novelty_scores = [self._novelty.current_score(e) for e in emb_stack]
                    avg_novelty = float(np.mean(novelty_scores))
                else:
                    avg_novelty = 0.0
            else:
                avg_novelty = 0.0
            result[did] = {
                "avg_confidence": float(np.mean(s["confidences"])) if s["confidences"] else 0.5,
                "avg_novelty": round(avg_novelty, 3),
            }
        return result