import numpy as np
import time
from ..types import DetectionMethod


class CentroidManager:
    def __init__(self, config):
        pc = config["preferences"]
        self.min_examples = pc["centroid_min_examples"]
        self.refresh_days = pc["centroid_refresh_days"]
        self.max_age_days = pc["centroid_max_age_days"]
        self._config = config
        self._positive_examples = []
        self._negative_examples = []
        self._positive_centroid = None
        self._negative_centroid = None
        self._example_count = 0
        self._last_computed_at = 0.0
        self._stale_threshold = config["health"].get("centroid_stale_days", 60) * 86400

    @property
    def ready(self):
        return self._positive_centroid is not None and self._negative_centroid is not None

    @property
    def example_count(self):
        return self._example_count

    @property
    def positive_centroid(self):
        return self._positive_centroid

    @property
    def negative_centroid(self):
        return self._negative_centroid

    @property
    def is_stale(self):
        if self._last_computed_at == 0:
            return False
        return (time.time() - self._last_computed_at) > self._stale_threshold

    def add_example(self, embedding, label):
        emb = np.asarray(embedding, dtype=np.float64).ravel()
        norm = float(np.linalg.norm(emb))
        if norm > 1e-10:
            emb = emb / norm
        if label in ("keep_this", "positive"):
            self._positive_examples.append(emb)
        elif label in ("dont_do_again", "negative"):
            self._negative_examples.append(emb)
        self._example_count += 1
        if len(self._positive_examples) >= self.min_examples and len(
            self._negative_examples
        ) >= max(10, self.min_examples // 5):
            self.recompute()

    def recompute(self):
        recomputed = False
        if len(self._positive_examples) > 0:
            pos_stack = np.stack(self._positive_examples[-500:])
            self._positive_centroid = np.mean(pos_stack, axis=0)
            n = float(np.linalg.norm(self._positive_centroid))
            if n > 1e-10:
                self._positive_centroid /= n
            recomputed = True
        if len(self._negative_examples) > 0:
            neg_stack = np.stack(self._negative_examples[-500:])
            self._negative_centroid = np.mean(neg_stack, axis=0)
            n = float(np.linalg.norm(self._negative_centroid))
            if n > 1e-10:
                self._negative_centroid /= n
            recomputed = True
        if recomputed:
            self._last_computed_at = time.time()

    def classify(self, embedding):
        emb = np.asarray(embedding, dtype=np.float64).ravel()
        norm = float(np.linalg.norm(emb))
        if norm > 1e-10:
            emb = emb / norm
        result = {
            "type": None,
            "confidence": 0.0,
            "method": DetectionMethod.HEURISTIC,
        }
        if not self.ready:
            return result
        pos_sim = float(np.dot(emb, self._positive_centroid))
        neg_sim = float(np.dot(emb, self._negative_centroid))
        if pos_sim > neg_sim and pos_sim > 0.5:
            result["type"] = "keep_this"
            result["confidence"] = float(pos_sim)
            result["method"] = DetectionMethod.EMBEDDING
        elif neg_sim > pos_sim and neg_sim > 0.5:
            result["type"] = "dont_do_again"
            result["confidence"] = float(neg_sim)
            result["method"] = DetectionMethod.EMBEDDING
        return result

    def get_weights(self):
        return {
            "edge_distance": 1.0,
            "reward_signal": 1.0,
            "annotation_weight": 1.0,
        }

    def should_refresh(self):
        if self._last_computed_at == 0:
            return True
        days_since = (time.time() - self._last_computed_at) / 86400.0
        return days_since > self.refresh_days

    def trim_old_examples(self):
        max_age_seconds = self.max_age_days * 86400
        keep_count = max(self.min_examples, len(self._positive_examples) // 2)
        if len(self._positive_examples) > keep_count * 2:
            self._positive_examples = self._positive_examples[-keep_count:]
        if len(self._negative_examples) > keep_count * 2:
            self._negative_examples = self._negative_examples[-keep_count:]

    def to_dict(self):
        return {
            "positive_centroid": self._positive_centroid.tolist()
            if self._positive_centroid is not None else None,
            "negative_centroid": self._negative_centroid.tolist()
            if self._negative_centroid is not None else None,
            "example_count": self._example_count,
            "last_computed_at": self._last_computed_at,
            "ready": self.ready,
        }

    def from_dict(self, data):
        if data.get("positive_centroid"):
            self._positive_centroid = np.array(
                data["positive_centroid"], dtype=np.float64
            )
        if data.get("negative_centroid"):
            self._negative_centroid = np.array(
                data["negative_centroid"], dtype=np.float64
            )
        self._example_count = data.get("example_count", 0)
        self._last_computed_at = data.get("last_computed_at", 0.0)
