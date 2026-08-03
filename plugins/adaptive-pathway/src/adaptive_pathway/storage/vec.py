import numpy as np

_INITIAL_CAPACITY = 16

class VectorIndex:
    def __init__(self):
        self._ids: list[str] = []
        self._embeddings: np.ndarray = np.empty((0, 0), dtype=np.float32)
        self._norms: np.ndarray = np.empty((0,), dtype=np.float32)
        # Active row count. `_embeddings`/`_norms` may have spare, uninitialized
        # capacity beyond this (see `add`) — always slice to `:self._count`
        # before using them, never trust their raw `.shape[0]`.
        self._count = 0

    def build(self, ids, embeddings):
        self._ids = list(ids)
        self._embeddings = np.asarray(embeddings, dtype=np.float32)
        self._norms = np.linalg.norm(self._embeddings, axis=1)
        self._norms[self._norms < 1e-12] = 1.0
        self._count = len(self._ids)

    def search(self, query, k=5):
        if self._count == 0:
            return []
        q = np.asarray(query, dtype=np.float32).ravel()
        q_norm = float(np.linalg.norm(q))
        if q_norm < 1e-12:
            return []
        q = q / q_norm
        embeddings = self._embeddings[: self._count]
        norms = self._norms[: self._count]
        scores = (embeddings @ q) / norms
        if k >= self._count:
            top_idx = np.argsort(-scores)
        else:
            top_idx = np.argpartition(-scores, k - 1)[:k]
            top_idx = top_idx[np.argsort(-scores[top_idx])]
        return [(self._ids[i], float(scores[i])) for i in top_idx]

    def add(self, id_, embedding):
        # Amortized-doubling growth, not a full matrix rebuild per call —
        # `np.vstack`-ing onto the whole array on every single `add()` copied
        # the entire matrix each time (O(n) per add, O(n^2) over a session of
        # n adds). Growing capacity in doubling steps keeps the *average*
        # cost of an add O(1), same idea as `list.append`'s own amortized
        # growth.
        emb = np.asarray(embedding, dtype=np.float32).ravel()
        dim = emb.shape[0]

        if self._embeddings.shape[1:2] != (dim,) or self._embeddings.shape[0] < self._count + 1:
            capacity = self._embeddings.shape[0] if self._embeddings.ndim == 2 else 0
            if self._embeddings.shape[1:2] != (dim,):
                # Dimensionality changed (or this is the very first vector) —
                # nothing usable to carry over.
                capacity = 0
            new_capacity = max(capacity * 2, self._count + 1, _INITIAL_CAPACITY)
            grown_embeddings = np.empty((new_capacity, dim), dtype=np.float32)
            grown_norms = np.empty((new_capacity,), dtype=np.float32)
            if capacity > 0:
                grown_embeddings[: self._count] = self._embeddings[: self._count]
                grown_norms[: self._count] = self._norms[: self._count]
            self._embeddings = grown_embeddings
            self._norms = grown_norms

        self._embeddings[self._count] = emb
        self._norms[self._count] = float(np.linalg.norm(emb)) or 1.0
        self._ids.append(id_)
        self._count += 1

    def __len__(self):
        return self._count
