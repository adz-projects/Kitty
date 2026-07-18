import numpy as np
import time
from enum import Enum
from ..types import AnnotationType, DetectionMethod


class PreferenceIntensity(Enum):
    MILD = "mild"
    MODERATE = "moderate"
    STRONG = "strong"


class PreferenceDetector:
    def __init__(self, config):
        pcfg = config["preferences"]
        self.centroid_min_examples = pcfg["centroid_min_examples"]
        self.centroid_refresh_days = pcfg["centroid_refresh_days"]
        self.centroid_max_age_days = pcfg["centroid_max_age_days"]
        self.embedding_confidence_threshold = pcfg["embedding_confidence_threshold"]
        self.embedding_uncertainty_threshold = pcfg["embedding_uncertainty_threshold"]
        self.behavioral_confirmation_wait_turns = pcfg["behavioral_confirmation_wait_turns"]
        self.heuristic_fallback_confidence = pcfg["heuristic_fallback_confidence"]
        self.intensity_mild = pcfg["intensity_mild"]
        self.intensity_moderate = pcfg["intensity_moderate"]
        self.config = config
        self.pcfg = pcfg
        self._positive_embeddings = []
        self._negative_embeddings = []
        self._positive_centroid = None
        self._negative_centroid = None
        self._example_count = 0
        self._last_computed_at = 0.0
        self._pending_confirmations = {}
        self._multi_turn_buffer = []
        self._penalty_timestamps = {}

    @property
    def centroids_ready(self):
        return self._example_count >= self.centroid_min_examples and self._positive_centroid is not None

    def add_labeled_example(self, embedding, label, intensity=0.5, edge_id=None):
        emb = np.asarray(embedding, dtype=np.float64).ravel()
        norm = float(np.linalg.norm(emb))
        if norm > 1e-10:
            emb = emb / norm
        if label in ("keep_this", "positive"):
            self._positive_embeddings.append(emb)
        elif label in ("dont_do_again", "negative"):
            self._negative_embeddings.append(emb)
            if edge_id:
                self._penalty_timestamps[edge_id] = time.time()
        self._example_count += 1
        if len(self._positive_embeddings) >= self.centroid_min_examples and len(self._negative_embeddings) >= 10:
            self._recompute_centroids()

    def _recompute_centroids(self):
        if len(self._positive_embeddings) > 0:
            pos = np.stack(self._positive_embeddings[-500:])
            self._positive_centroid = np.mean(pos, axis=0)
            self._positive_centroid /= float(np.linalg.norm(self._positive_centroid)) or 1.0
        if len(self._negative_embeddings) > 0:
            neg = np.stack(self._negative_embeddings[-500:])
            self._negative_centroid = np.mean(neg, axis=0)
            self._negative_centroid /= float(np.linalg.norm(self._negative_centroid)) or 1.0
        self._last_computed_at = time.time()

    def detect(self, context_embedding, edge_id=None):
        emb = np.asarray(context_embedding, dtype=np.float64).ravel()
        norm = float(np.linalg.norm(emb))
        if norm > 1e-10:
            emb = emb / norm
        result = {
            "type": None,
            "intensity": PreferenceIntensity.MILD,
            "confidence": 0.0,
            "detection_method": DetectionMethod.HEURISTIC,
            "reward_weight": 0.0,
        }
        if self.centroids_ready:
            result = self._detect_embedding(emb, edge_id)
        if result["type"] is None:
            result = self._detect_behavioral(emb, edge_id)
        if result["type"] is None:
            result = self._detect_heuristic(emb)
        return result

    def _detect_embedding(self, emb, edge_id=None):
        pos_sim = float(np.dot(emb, self._positive_centroid)) if self._positive_centroid is not None else 0.0
        neg_sim = float(np.dot(emb, self._negative_centroid)) if self._negative_centroid is not None else 0.0
        if pos_sim >= self.embedding_confidence_threshold:
            intensity = self._classify_intensity(pos_sim)
            return {
                "type": AnnotationType.KEEP_THIS,
                "intensity": intensity,
                "confidence": float(pos_sim),
                "detection_method": DetectionMethod.EMBEDDING,
                "reward_weight": self._reward_weight("keep_this", intensity, edge_id),
            }
        if neg_sim >= self.embedding_confidence_threshold:
            intensity = self._classify_intensity(neg_sim)
            rw = self._reward_weight("dont_do_again", intensity, edge_id)
            return {
                "type": AnnotationType.DONT_DO_AGAIN,
                "intensity": intensity,
                "confidence": float(neg_sim),
                "detection_method": DetectionMethod.EMBEDDING,
                "reward_weight": rw,
            }
        # Neither centroid was confident enough to commit — if one is at
        # least plausible, stage it for behavioral confirmation on a later turn.
        best_sim = max(pos_sim, neg_sim)
        if best_sim >= self.embedding_uncertainty_threshold:
            candidate = AnnotationType.KEEP_THIS if pos_sim >= neg_sim else AnnotationType.DONT_DO_AGAIN
            self.stage_for_confirmation(candidate, emb)
        return {"type": None, "intensity": PreferenceIntensity.MILD, "confidence": 0.0,
                "detection_method": DetectionMethod.EMBEDDING, "reward_weight": 0.0}

    def _detect_behavioral(self, emb, edge_id=None):
        for key, pending in list(self._pending_confirmations.items()):
            if pending["turns_remaining"] <= 0:
                confirmed = self._check_confirmation(emb, pending, edge_id)
                if confirmed:
                    del self._pending_confirmations[key]
                    return confirmed
                del self._pending_confirmations[key]
        return {"type": None, "intensity": PreferenceIntensity.MILD, "confidence": 0.0,
                "detection_method": DetectionMethod.BEHAVIORAL, "reward_weight": 0.0}

    def _check_confirmation(self, emb, pending, edge_id=None):
        if pending["candidate_type"] is None:
            return None
        centroids_ready = self._positive_centroid is not None
        if centroids_ready and pending["candidate_type"] == AnnotationType.KEEP_THIS:
            pos_sim = float(np.dot(emb, self._positive_centroid))
            if pos_sim >= self.embedding_confidence_threshold:
                intensity = self._classify_intensity(pos_sim)
                return {
                    "type": AnnotationType.KEEP_THIS,
                    "intensity": intensity,
                    "confidence": float(pos_sim),
                    "detection_method": DetectionMethod.HYBRID,
                    "reward_weight": self._reward_weight("keep_this", intensity, edge_id),
                    "multi_turn_resolved": True,
                }
            return {"type": None, "intensity": PreferenceIntensity.MILD, "confidence": pos_sim,
                    "detection_method": DetectionMethod.HEURISTIC, "reward_weight": 0.0}
        if centroids_ready and pending["candidate_type"] == AnnotationType.DONT_DO_AGAIN:
            neg_sim = float(np.dot(emb, self._negative_centroid))
            if neg_sim >= self.embedding_confidence_threshold:
                intensity = self._classify_intensity(neg_sim)
                rw = self._reward_weight("dont_do_again", intensity, edge_id)
                return {
                    "type": AnnotationType.DONT_DO_AGAIN,
                    "intensity": intensity,
                    "confidence": float(neg_sim),
                    "detection_method": DetectionMethod.HYBRID,
                    "reward_weight": rw,
                    "multi_turn_resolved": True,
                }
        return None

    def _detect_heuristic(self, emb):
        if self._positive_centroid is not None:
            pos_sim = float(np.dot(emb, self._positive_centroid))
            neg_sim = float(np.dot(emb, self._negative_centroid)) if self._negative_centroid is not None else 0.0
            if pos_sim > neg_sim and pos_sim > 0.3:
                return {
                    "type": AnnotationType.KEEP_THIS,
                    "intensity": PreferenceIntensity.MILD,
                    "confidence": self.heuristic_fallback_confidence,
                    "detection_method": DetectionMethod.HEURISTIC,
                    "reward_weight": self.pcfg["keep_this_weight_mild"],
                }
        return {"type": None, "intensity": PreferenceIntensity.MILD, "confidence": 0.0,
                "detection_method": DetectionMethod.HEURISTIC, "reward_weight": 0.0}

    def stage_for_confirmation(self, candidate_type, embedding):
        emb = np.asarray(embedding, dtype=np.float64).ravel()
        norm = float(np.linalg.norm(emb))
        if norm > 1e-10:
            emb = emb / norm
        key = str(hash(emb.tobytes()))
        self._pending_confirmations[key] = {
            "candidate_type": candidate_type,
            "embedding": emb,
            "turns_remaining": self.behavioral_confirmation_wait_turns,
        }

    def tick_pending(self):
        to_delete = []
        for key, pending in self._pending_confirmations.items():
            pending["turns_remaining"] -= 1
            # Never resolved by a matching record_annotation() call — drop it
            # rather than let _pending_confirmations grow without bound.
            if pending["turns_remaining"] < -10:
                to_delete.append(key)
        for key in to_delete:
            del self._pending_confirmations[key]

    def _classify_intensity(self, similarity):
        if similarity >= self.intensity_moderate:
            return PreferenceIntensity.STRONG
        if similarity >= self.intensity_mild:
            return PreferenceIntensity.MODERATE
        return PreferenceIntensity.MILD

    def _reward_weight(self, pref_type, intensity, edge_id=None):
        if pref_type == "keep_this":
            if intensity == PreferenceIntensity.STRONG:
                return self.pcfg["keep_this_weight_strong"]
            if intensity == PreferenceIntensity.MODERATE:
                return self.pcfg["keep_this_weight_moderate"]
            return self.pcfg["keep_this_weight_mild"]
        base = 0.0
        if intensity == PreferenceIntensity.STRONG:
            base = self.pcfg["dont_do_again_weight_strong"]
        elif intensity == PreferenceIntensity.MODERATE:
            base = self.pcfg["dont_do_again_weight_moderate"]
        else:
            base = self.pcfg["dont_do_again_weight_mild"]
        half_life_days = self.pcfg.get("negative_pref_half_life_days", 0)
        if half_life_days > 0 and edge_id and edge_id in self._penalty_timestamps:
            age = time.time() - self._penalty_timestamps[edge_id]
            decay = 0.5 ** (age / (half_life_days * 86400))
            return base * decay
        return base

    def get_lambda_boost(self, pref_type, intensity):
        if pref_type != "dont_do_again":
            return {"sessions": 0}
        if intensity == PreferenceIntensity.STRONG:
            if self.pcfg.get("lambda_boost_plus_two_sessions", True):
                return {"sessions": 3}
            return {"sessions": 2}
        if intensity == PreferenceIntensity.MODERATE:
            if self.pcfg.get("lambda_boost_plus_one_session", True):
                return {"sessions": 2}
        if self.pcfg.get("lambda_boost_session_only", True):
            return {"sessions": 1}
        return {"sessions": 0}
