import numpy as np
import time
from .types import GraphHealth, HealthIssue


class HealthChecker:
    def __init__(self, config, get_state_fn, get_edges_fn, get_novelty_fn,
                 get_ensemble_fn, get_domains_fn, hasher, detector):
        self._config = config
        self._hc = config["health"]
        self._get_state = get_state_fn
        self._get_edges = get_edges_fn
        self._get_novelty = get_novelty_fn
        self._get_ensemble = get_ensemble_fn
        self._get_domains = get_domains_fn
        self._hasher = hasher
        self._detector = detector
        self._last_override_rate = 0.0
        self._override_history = []
        self._prediction_log_snapshot = None

    def run_full_check(self):
        issues = []
        issues.extend(self._check_features())
        issues.extend(self._check_novelty())
        issues.extend(self._check_ensemble())
        issues.extend(self._check_graph())
        issues.extend(self._check_preferences())
        issues.extend(self._check_tiers())
        issues.extend(self._check_override_rates())
        return issues

    def get_graph_health(self):
        edges = self._get_edges()
        total = len(edges)
        confidences = [e.confidence for e in edges if e.confidence is not None]
        high_conf = sum(1 for c in confidences if c >= 0.8)
        high_conf_pct = high_conf / max(total, 1)
        tiers = {"hot": 0, "warm": 0, "cold": 0}
        for e in edges:
            t = getattr(e, "tier", "cold")
            if t in tiers:
                tiers[t] += 1
        dimensionality = {
            "utilization": round(float(self._hasher.utilization), 3),
            "collision_rate": round(float(self._hasher.collision_rate), 3),
            "n_buckets": self._hasher.n_buckets,
        }
        ensemble = self._get_ensemble()
        ensemble_health = {
            "schism_state": getattr(ensemble, "schism_state", None),
            "plateau_risk_score": getattr(ensemble, "plateau_risk_score", 0.0),
            "ig_weight": getattr(ensemble, "ig_weight", 0.15),
            "diversity_mode": getattr(ensemble, "ensemble_diversity_mode", False),
        }
        novelty_vals = self._get_novelty()[-100:] if self._get_novelty() else []
        novelty_health = {
            "variance": round(float(np.var(novelty_vals)), 5) if len(novelty_vals) > 1 else 0.0,
            "mean": round(float(np.mean(novelty_vals)), 3) if novelty_vals else 0.0,
            "samples": len(novelty_vals),
        }
        hotspot_details = []
        for e in edges:
            if e.confidence and e.confidence >= 0.9 and e.override_rate and e.override_rate > 0.3:
                hotspot_details.append({
                    "edge_id": e.id,
                    "primitive": e.semantic_primitive,
                    "confidence": float(e.confidence),
                    "override_rate": float(e.override_rate),
                })
        return GraphHealth(
            total_edges=total,
            high_confidence_pct=round(high_conf_pct, 3),
            flagged_hotspots=len(hotspot_details),
            last_override_rate=round(self._last_override_rate, 3),
        blocking_issues=any(
            i.severity == "critical" for i in self.run_full_check()
        ),
            dimensionality_health=dimensionality,
            ensemble_health=ensemble_health,
            novelty_health=novelty_health,
            tier_distribution=tiers,
            hotspot_details=hotspot_details,
        )

    def update_metrics(self, action_id, reward):
        self._override_history.append({
            "action_id": str(action_id),
            "reward": float(reward),
            "timestamp": time.time(),
        })
        if len(self._override_history) > 100:
            self._override_history.pop(0)

    def _check_features(self):
        issues = []
        collision = float(self._hasher.collision_rate)
        if collision > self._hc.get("collision_rate_warning", 0.15):
            issues.append(HealthIssue(
                severity="warning", component="features",
                message=f"Feature collision rate {round(collision, 3)} exceeds threshold",
                details={"collision_rate": round(collision, 3),
                         "threshold": self._hc["collision_rate_warning"]},
            ))
        utilization = float(self._hasher.utilization)
        if utilization > 0.80:
            issues.append(HealthIssue(
                severity="warning", component="features",
                message=f"Feature space utilization {round(utilization, 3)} is high",
                details={"utilization": round(utilization, 3)},
            ))
        return issues

    def _check_novelty(self):
        issues = []
        novelty_vals = self._get_novelty()[-100:] if self._get_novelty() else []
        if len(novelty_vals) < 50:
            return issues
        nv = self._hc
        stale_count = sum(1 for v in novelty_vals if v < nv.get("stale_novelty_threshold", 0.05))
        stale_pct = stale_count / len(novelty_vals)
        if stale_pct > nv.get("stale_novelty_pct", 0.90):
            issues.append(HealthIssue(
                severity="warning", component="novelty",
                message=f"Novelty scores stale: {round(stale_pct, 3)} below threshold",
                details={"stale_percentage": round(stale_pct, 3),
                         "threshold": nv["stale_novelty_pct"]},
            ))
        variance = float(np.var(novelty_vals))
        if variance < nv.get("novelty_variance_min", 0.1) and len(novelty_vals) > 50:
            issues.append(HealthIssue(
                severity="info", component="novelty",
                message=f"Novelty variance low ({round(variance, 5)}) — possible saturation",
                details={"variance": round(variance, 5)},
            ))
        return issues

    def _check_ensemble(self):
        issues = []
        ensemble = self._get_ensemble()
        state_val = getattr(ensemble, "schism_state", None)
        if state_val is not None:
            state_str = state_val.value if hasattr(state_val, "value") else str(state_val)
            if state_str not in ("none", "resolved"):
                issues.append(HealthIssue(
                    severity="warning", component="ensemble",
                    message=f"Ensemble schism state: {state_str}",
                    details={"state": state_str},
                ))
        return issues

    def _check_graph(self):
        issues = []
        edges = self._get_edges()
        if len(edges) == 0:
            issues.append(HealthIssue(
                severity="info", component="graph",
                message="Graph is empty — cold start",
                details={},
            ))
            return issues
        confidences = [e.confidence for e in edges if e.confidence is not None]
        if confidences:
            mean_conf = float(np.mean(confidences))
            if mean_conf < 0.3:
                issues.append(HealthIssue(
                    severity="info", component="graph",
                    message=f"Low average confidence ({round(mean_conf, 3)})",
                    details={"mean_confidence": round(mean_conf, 3)},
                ))
        # Confidence inversion: check if any high-frequency edge has
        # lower confidence than any low-frequency edge (limited to first 50
        # to avoid O(n^2) on large graphs).
        sample = edges[:50]
        for i, ea in enumerate(sample):
            for j, eb in enumerate(sample):
                if i >= j:
                    continue
                if (ea.confidence is not None and eb.confidence is not None and
                    ea.confidence < eb.confidence and
                    (ea.frequency or 0) > (eb.frequency or 0) and
                    (ea.confidence or 0.5) < 0.4):
                    issues.append(HealthIssue(
                        severity="info", component="graph",
                        message=f"Confidence inversion between {ea.id} and {eb.id}",
                        details={"edge_a": ea.id, "edge_b": eb.id},
                    ))
                    break
        return issues

    def _check_preferences(self):
        issues = []
        if self._detector and hasattr(self._detector, "centroids_ready"):
            if not self._detector.centroids_ready and getattr(self._detector, "_example_count", 0) > 0:
                issues.append(HealthIssue(
                    severity="info", component="preferences",
                    message=f"Preference centroids not ready ({self._detector._example_count} examples)",
                    details={"example_count": self._detector._example_count,
                             "min_required": getattr(self._detector, "centroid_min_examples", 50)},
                ))
        return issues

    def _check_tiers(self):
        issues = []
        edges = self._get_edges()
        tiers = {"hot": 0, "warm": 0, "cold": 0}
        for e in edges:
            t = getattr(e, "tier", "cold")
            if t in tiers:
                tiers[t] += 1
        total = sum(tiers.values())
        if total > 0 and tiers.get("cold", 0) / total > 0.8:
            issues.append(HealthIssue(
                severity="info", component="tiers",
                message=f"High proportion of cold edges ({tiers['cold']}/{total})",
                details={"tier_distribution": tiers},
            ))
        return issues

    def _check_override_rates(self):
        issues = []
        sessions = self._hc.get("override_rate_spike_sessions", 3)
        spike_threshold = self._hc.get("override_rate_spike", 0.40)
        if len(self._override_history) < 10:
            return issues
        recent = self._override_history[-10:]
        negative = sum(1 for r in recent if r["reward"] < 0)
        rate = negative / len(recent)
        self._last_override_rate = rate
        if rate > spike_threshold and len(self._override_history) >= 10 * sessions:
            issues.append(HealthIssue(
                severity="warning", component="override",
                message=f"Override rate spike: {round(rate, 3)}",
                details={"override_rate": round(rate, 3),
                         "threshold": spike_threshold},
            ))
        return issues
