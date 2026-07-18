import numpy as np

class VectorIndex:
    def __init__(self):
        self._ids: list[str] = []
        self._embeddings: np.ndarray = np.empty((0, 0), dtype=np.float32)
        self._norms: np.ndarray = np.empty((0,), dtype=np.float32)

    def build(self, ids, embeddings):
        self._ids = list(ids)
        self._embeddings = np.asarray(embeddings, dtype=np.float32)
        self._norms = np.linalg.norm(self._embeddings, axis=1)
        self._norms[self._norms < 1e-12] = 1.0

    def search(self, query, k=5):
        if len(self._ids) == 0:
            return []
        q = np.asarray(query, dtype=np.float32).ravel()
        q_norm = float(np.linalg.norm(q))
        if q_norm < 1e-12:
            return []
        q = q / q_norm
        scores = (self._embeddings @ q) / self._norms
        if k >= len(self._ids):
            top_idx = np.argsort(-scores)
        else:
            top_idx = np.argpartition(-scores, k - 1)[:k]
            top_idx = top_idx[np.argsort(-scores[top_idx])]
        return [(self._ids[i], float(scores[i])) for i in top_idx]

    def add(self, id_, embedding):
        emb = np.asarray(embedding, dtype=np.float32).ravel()
        if len(self._ids) == 0:
            self._ids = [id_]
            self._embeddings = emb[np.newaxis, :]
            self._norms = np.array([float(np.linalg.norm(emb)) or 1.0], dtype=np.float32)
        else:
            self._ids.append(id_)
            self._embeddings = np.vstack([self._embeddings, emb])
            self._norms = np.append(self._norms, float(np.linalg.norm(emb)) or 1.0)

    def __len__(self):
        return len(self._ids)
