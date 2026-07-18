import numpy as np
import time
from .thompson import ThompsonLinUCB
from .info_gain import InformationGainModel
from .paradigm_challenge import ParadigmChallengeModel
from ..types import SchismAlert, SchismState, PlateauRisk

class BootstrapEnsemble:
    def __init__(self, config, get_domain_fn, get_edge_fn):
        self.config = config
        ec = config["ensemble"]
        sc = config["schism"]
        pr = config["plateau_risk"]

        self.n_actions = config["thompson"]["max_action_buckets"]
        self.d_features = config["thompson"]["feature_buckets"]
        self.ig_weight_min = ec["ig_weight_min"]
        self.ig_weight_max = ec["ig_weight_max"]
        self.pc_weight = ec["pc_weight"]
        self.bootstrap_prob = ec["bootstrap_probability"]
        self.schism_models = sc["models_to_check"]
        self.n_schism = len(self.schism_models)

        self.models = [
            ThompsonLinUCB(self.n_actions, self.d_features,
                          noise_var=ec["noise_variance_standard"])
            for _ in range(4)
        ]
        self.models.append(InformationGainModel(self.n_actions, self.d_features,
                                                 noise_var=ec["noise_variance_ig"]))
        self.models.append(ParadigmChallengeModel(config, get_domain_fn, get_edge_fn))
        self.ig_model_index = 4
        self.pc_model_index = 5

        self.plateau_risk_score = 0.0
        self.plateau_risk_components = {"entropy": 0.0, "diversity": 0.0,
                                         "novelty": 0.0, "agreement": 0.0}
        self.risk_signals = pr["signal_weights"]

        self.min_hours = sc["min_hours_between_checks"]
        self.check_interval = sc["check_call_interval"]
        self.extended_interval = sc["extended_interval"]
        self.within_thresh = sc["within_agreement_threshold"]
        self.between_thresh = sc["between_agreement_threshold"]
        self.drop_thresh = sc["agreement_drop_threshold"]
        self.similarity_skip = sc["agreement_similarity_skip"]
        self.stable_std = sc["stable_trend_std"]
        self.log_size = sc["prediction_log_size"]
        self.min_preds = sc["min_predictions_for_check"]
        self.auto_resolve_days = sc["auto_resolve_days"]
        self.domain_suppress = sc["domain_split_suppression"]
        self.min_faction_size = sc["min_faction_size"]

        self.call_counter = 0
        self.prediction_log = [[] for _ in range(6)]
        self.agreement_snapshots = []
        self.last_check_time = None
        self.extended_interval_active = False
        self.schism_state = SchismState.NONE
        self.schism_data = None
        self.schism_detected_at = None
        self.ensemble_diversity_mode = False
        self.last_updated = {}

    @property
    def ig_weight(self):
        return self.ig_weight_min + (self.ig_weight_max - self.ig_weight_min) * self.plateau_risk_score

    def sample(self, action_id, context):
        ig_w = self.ig_weight
        std_w = (1.0 - ig_w - self.pc_weight) / 4
        weights = [std_w] * 4 + [ig_w, self.pc_weight]
        samples = [m.sample(action_id, context) for m in self.models]
        return float(np.dot(samples, weights)), samples

    def max_sigma(self, action_id, context):
        # Epistemic uncertainty for a bucket: the largest posterior sigma
        # among the 4 standard Thompson models. Used to guarantee one
        # "we genuinely don't know" hint per decide() call, independent of
        # preference score.
        return max(self.models[i].predict(action_id, context)[1] for i in range(4))

    def apply_confidence_decay(self, base_half_life_hours, rate_multiplier, max_decay_fraction=0.3):
        # Widens Thompson posterior variance (blends A_inv toward identity)
        # for buckets that haven't been updated in a while, so old
        # preferences must re-earn confidence via re-sampling rather than
        # staying permanently pinned. Mean estimates (`b`) are untouched —
        # only uncertainty grows, which Thompson sampling naturally turns
        # into more exploration for stale buckets.
        now = time.time()
        for bucket, ts in self.last_updated.items():
            age_hours = (now - ts) / 3600
            if age_hours <= 0:
                continue
            target_decay = 1.0 - 0.5 ** (age_hours / base_half_life_hours)
            decay_factor = min(target_decay * rate_multiplier, max_decay_fraction)
            if decay_factor <= 0:
                continue
            eye = np.eye(self.d_features, dtype=np.float64)
            for i in list(range(4)) + [self.ig_model_index]:
                m = self.models[i]
                m.A_inv[bucket] = m.A_inv[bucket] * (1 - decay_factor) + eye * decay_factor

    def agreement(self, action_id, context):
        preds = [self.models[i].predict(action_id, context)[0] for i in range(4)]
        ig_pred, _ = self.models[4].predict(action_id, context)
        pc_score = self.models[5].sample(action_id, context)
        return {
            "mean": float(np.mean(preds)),
            "std": float(np.std(preds)),
            "predictions": preds,
            "disagree": float(np.std(preds)) > 0.2,
            "ig_model_score": ig_pred,
            "pc_model_score": pc_score,
        }

    def update(self, action_id, context, reward, domain_id=None):
        self.last_updated[action_id] = time.time()
        for i in range(4):
            if np.random.random() < self.bootstrap_prob:
                self.models[i].update(action_id, context, reward)
        self.models[4].update(action_id, context, reward)

        for i, model in enumerate(self.models):
            pred, _ = model.predict(action_id, context)
            self.prediction_log[i].append({
                "action_id": action_id,
                "predicted_value": pred,
                "domain_id": domain_id,
                "timestamp": time.time(),
            })
            if len(self.prediction_log[i]) > self.log_size:
                self.prediction_log[i].pop(0)

        current_matrix = self._compute_agreement_matrix()
        self.agreement_snapshots.append(current_matrix)
        if len(self.agreement_snapshots) > 10:
            self.agreement_snapshots.pop(0)

        self.call_counter += 1
        interval = self.extended_interval if self.extended_interval_active else self.check_interval
        if self.call_counter >= interval:
            self.call_counter = 0
            if self._should_check():
                return self._detect()
        return None

    def evaluate_plateau_risk(self, action_history, novelty_history):
        pr = self.risk_signals
        er = self._entropy_risk(action_history)
        dr = self._diversity_risk(action_history)
        nr = self._novelty_risk(novelty_history)
        ar = self._agreement_risk()
        score = min(1.0, max(0.0, pr["entropy"] * er + pr["diversity"] * dr +
                             pr["novelty"] * nr + pr["agreement"] * ar))
        prev = self.plateau_risk_score
        self.plateau_risk_score = score
        self.plateau_risk_components = {"entropy": er, "diversity": dr,
                                         "novelty": nr, "agreement": ar}
        trend = "rising" if score > prev + 0.05 else ("falling" if score < prev - 0.05 else "stable")
        return PlateauRisk(score=round(score, 3), entropy_risk=round(er, 3),
                          diversity_risk=round(dr, 3), novelty_risk=round(nr, 3),
                          agreement_risk=round(ar, 3), trend=trend,
                          ig_weight=round(self.ig_weight, 3))

    def _entropy_risk(self, history):
        cfg = self.config["plateau_risk"]
        w, s = cfg["entropy_window"], cfg["entropy_stride"]
        if len(history) < w + s * 2:
            return 0.0
        entropies = []
        for i in range(len(history) - w, -1, -s):
            chunk = history[i:i + w]
            if len(chunk) < w:
                continue
            counts = {}
            for a in chunk:
                counts[a] = counts.get(a, 0) + 1
            t = len(chunk)
            ent = -sum((c / t) * np.log(max(c / t, 1e-10)) for c in counts.values())
            n_u = len(set(chunk))
            entropies.insert(0, ent / np.log(max(n_u, 2)) if n_u > 1 else 1.0)
            if len(entropies) >= 4:
                break
        if len(entropies) < 2:
            return 0.0
        x_steps = [i * s for i in range(len(entropies))]
        if np.var(x_steps) < 1e-10:
            return 0.0
        trend = np.polyfit(x_steps, entropies, 1)[0]
        return min(1.0, max(0.0, -trend * cfg["entropy_risk_scale"]))

    def _diversity_risk(self, history):
        cfg = self.config["plateau_risk"]
        w, s = cfg["entropy_window"], cfg["entropy_stride"]
        if len(history) < w * 2:
            return 0.0
        uniques = []
        for i in range(len(history) - w, -1, -s):
            chunk = history[i:i + w]
            if len(chunk) < w:
                continue
            uniques.insert(0, len(set(chunk)))
            if len(uniques) >= 3:
                break
        if len(uniques) < 3 or uniques[-3] <= cfg["diversity_min_baseline"]:
            return 0.0
        ratio = uniques[-1] / max(uniques[-3], 1)
        return 1.0 - min(1.0, max(0.0, ratio))

    def _novelty_risk(self, novelty_history):
        cfg = self.config["plateau_risk"]
        if len(novelty_history) < 40:
            return 0.0
        recent = novelty_history[-30:]
        mid = novelty_history[-60:-30] if len(novelty_history) >= 60 else [1.0] * 30
        v1 = (recent[-1] - recent[0]) / max(len(recent), 1)
        v0 = (mid[-1] - mid[0]) / max(len(mid), 1) if mid else 0.0
        return min(1.0, max(0.0, -(v1 - v0) * cfg["novelty_accel_scale"]))

    def _agreement_risk(self):
        cfg = self.config["plateau_risk"]
        if len(self.agreement_snapshots) < 3:
            return 0.0
        stds = [self._avg_pairwise_std(m) for m in self.agreement_snapshots[-3:]]
        if stds[-3] <= cfg["agreement_min_std"]:
            return 0.0
        return 1.0 - min(1.0, max(0.0, stds[-1] / max(stds[-3], 1e-10)))

    def _avg_pairwise_std(self, matrix):
        n = len(matrix)
        vals = [matrix[i][j] for i in range(n) for j in range(n) if i != j]
        return float(np.std(vals)) if vals else 0.0

    def _should_check(self):
        # `agreement_snapshots` is owned exclusively by `update()` (single
        # append + cap-at-10 retention policy). This method only reads it —
        # it must NOT append its own copy of the current tick's matrix, or
        # it ends up comparing the just-appended snapshot against an
        # identical freshly-recomputed one (cosine similarity ~1.0 always),
        # which made the schism check skip almost every time.
        if self.last_check_time:
            if (time.time() - self.last_check_time) / 3600 < self.min_hours:
                return False
        if any(len(self.prediction_log[i]) < self.min_preds for i in self.schism_models):
            return False
        if len(self.agreement_snapshots) < 2:
            return False
        current = self.agreement_snapshots[-1]
        prev = self.agreement_snapshots[-2]
        prev_flat = prev.flatten()
        curr_flat = current.flatten()
        sim = np.dot(prev_flat, curr_flat) / (np.linalg.norm(prev_flat) * np.linalg.norm(curr_flat) + 1e-10)
        if sim > self.similarity_skip:
            if len(self.agreement_snapshots) >= 4:
                recent = self.agreement_snapshots[-4:]
                avgs = [self._avg_pairwise_std(m) for m in recent]
                if np.std(avgs) < self.stable_std:
                    self.extended_interval_active = True
            return False
        p = self._avg_pairwise_std(prev)
        c = self._avg_pairwise_std(current)
        if p - c < self.drop_thresh:
            return False
        self.last_check_time = time.time()
        return True

    def _compute_agreement_matrix(self):
        n = self.n_schism
        matrix = np.zeros((n, n))
        for i_idx, i in enumerate(self.schism_models):
            for j_idx, j in enumerate(self.schism_models):
                if i == j:
                    matrix[i_idx][j_idx] = 1.0
                    continue
                ri = [p["predicted_value"] for p in self.prediction_log[i][-10:]]
                rj = [p["predicted_value"] for p in self.prediction_log[j][-10:]]
                close = sum(1 for a, b in zip(ri, rj) if abs(a - b) < 0.15)
                matrix[i_idx][j_idx] = close / 10 if ri else 0.0
        return matrix

    def _detect(self):
        agreement = self._compute_agreement_matrix()
        n = self.n_schism
        best, best_score = None, 0
        for mask in range(1, (1 << n) - 1):
            fa = [i for i in range(n) if (mask >> i) & 1]
            fb = [i for i in range(n) if not ((mask >> i) & 1)]
            if len(fa) < self.min_faction_size or len(fb) < self.min_faction_size:
                continue
            wa = float(np.mean([agreement[i][j] for i in fa for j in fa if i != j]))
            wb = float(np.mean([agreement[i][j] for i in fb for j in fb if i != j]))
            bt = float(np.mean([agreement[i][j] for i in fa for j in fb]))
            if wa > self.within_thresh and wb > self.within_thresh and bt < self.between_thresh:
                score = min(wa, wb) - bt
                if score > best_score:
                    best_score = score
                    best = (fa, fb, wa, wb, bt)
        if best is None:
            return None
        fa, fb, wa, wb, bt = best
        # `fa`/`fb` are positional indices into `self.schism_models` (matching the
        # agreement matrix). Map them to actual model indices so every downstream
        # consumer (domain-split check, schism_data, SchismAlert, resolve) operates
        # in model-index space consistently, regardless of `models_to_check`.
        fa_models = [self.schism_models[i] for i in fa]
        fb_models = [self.schism_models[i] for i in fb]
        if self.domain_suppress and self._is_domain_split(fa_models, fb_models):
            return None
        self.schism_state = SchismState.DETECTED
        self.schism_detected_at = time.time()
        self.schism_data = {"fa": fa_models, "fb": fb_models,
                            "wa": wa, "wb": wb, "bt": bt}
        return SchismAlert(faction_a=fa_models, faction_b=fb_models,
                          within_a=wa, within_b=wb,
                          between=bt, faction_a_models=len(fa_models),
                          faction_b_models=len(fb_models),
                          detected_at=time.strftime("%Y-%m-%dT%H:%M:%SZ"))

    def _is_domain_split(self, fa, fb):
        da, db = set(), set()
        for i in fa:
            for p in self.prediction_log[i][-10:]:
                if p.get("domain_id"):
                    da.add(p["domain_id"])
        for i in fb:
            for p in self.prediction_log[i][-10:]:
                if p.get("domain_id"):
                    db.add(p["domain_id"])
        return len(da & db) == 0 and len(da) > 0 and len(db) > 0

    def resolve(self, keep_faction):
        if self.schism_state not in (SchismState.DETECTED, SchismState.REVIEWING):
            raise ValueError("No active schism")
        if keep_faction == "both":
            for m in range(len(self.schism_models)):
                model_idx = self.schism_models[m]
                if hasattr(self.models[model_idx], 'A_inv'):
                    for a in range(self.n_actions):
                        self.models[model_idx].A_inv[a] = (
                            self.models[model_idx].A_inv[a] * 1.3)
        else:
            d = self.schism_data
            winning = d["fa"] if keep_faction == "a" else d["fb"]
            losing = d["fb"] if keep_faction == "a" else d["fa"]
            for li in losing:
                wi = winning[np.random.randint(0, len(winning))]
                for a in range(self.n_actions):
                    self.models[li].A_inv[a] = self.models[wi].A_inv[a].copy()
                    self.models[li].b[a] = self.models[wi].b[a].copy()
        self.schism_state = SchismState.RESOLVED
