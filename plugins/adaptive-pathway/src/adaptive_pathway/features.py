import mmh3
import numpy as np

class FeatureHasher:
    def __init__(self, n_buckets=64):
        self.n_buckets = n_buckets
        self._counts = np.zeros(n_buckets, dtype=np.int64)
        self._total = 0

    def hash(self, key):
        h = mmh3.hash(str(key), seed=42) % self.n_buckets
        self._counts[h] += 1
        self._total += 1
        return h

    def get_features(self, action_id, metadata=None):
        features = np.zeros(self.n_buckets, dtype=np.float32)
        features[self.hash(action_id)] = 1.0
        if metadata:
            for k, v in metadata.items():
                features[self.hash(f"{k}:{v}")] = 0.5
        norm = float(np.linalg.norm(features))
        return features / norm if norm > 1e-10 else features

    def hash_embedding(self, embedding, n_active=12):
        emb = np.asarray(embedding, dtype=np.float32).ravel()
        emb_bytes = emb.tobytes()
        features = np.zeros(self.n_buckets, dtype=np.float32)
        for i in range(n_active):
            idx = mmh3.hash(emb_bytes, seed=(i * 7919 + 137)) % self.n_buckets
            features[idx] += 1.0
        norm = float(np.linalg.norm(features))
        return features / norm if norm > 1e-10 else features

    @property
    def utilization(self):
        return float(np.sum(self._counts > 0)) / self.n_buckets

    @property
    def collision_rate(self):
        if self._total == 0:
            return 0.0
        return 1.0 - (float(np.sum(self._counts > 0)) / max(self._total, 1))


class ActionBucketer:
    def __init__(self, max_buckets=20):
        self.max_buckets = max_buckets

    def get_bucket(self, key):
        if isinstance(key, (int, np.integer)):
            return int(key) % self.max_buckets
        return mmh3.hash(str(key), seed=137) % self.max_buckets
