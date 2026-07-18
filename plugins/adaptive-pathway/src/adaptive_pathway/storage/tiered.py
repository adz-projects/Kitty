import numpy as np
from .vec import VectorIndex
from ..types import EdgeInfo

class TieredCache:
    def __init__(self, config, vec_index, bucketer):
        self._config = config
        self._vec = vec_index
        self._bucketer = bucketer
        self._hot: dict[int, list[EdgeInfo]] = {}
        self._warm_ids: set[str] = set()
        self._pin_confidence = config["tiers"]["hot_pin_confidence"]

    def warm_from_db(self, edges, warm_embeddings):
        for edge in edges:
            bucket = self._bucketer.get_bucket(edge.semantic_primitive)
            if edge.confidence >= self._pin_confidence:
                edge.tier = "hot"
            if edge.tier == "hot":
                self._hot.setdefault(bucket, []).append(edge)
        ids, embs = [], []
        for eid, emb in warm_embeddings:
            ids.append(eid)
            embs.append(emb)
            self._warm_ids.add(eid)
        if ids:
            self._vec.build(ids, embs)

    def get_by_bucket(self, bucket_id):
        results = list(self._hot.get(bucket_id, []))
        return results

    def search_warm(self, query, k=5):
        if len(self._vec) == 0:
            return []
        return self._vec.search(query, k)

    def add_edge(self, edge_id, embedding, tier="hot"):
        if tier == "warm":
            self._vec.add(edge_id, embedding)
            self._warm_ids.add(edge_id)

    def add_hot(self, edge):
        """Register a newly-created edge mid-session, so `get_by_bucket` (the
        selector's hint source) sees it immediately rather than only after
        the next process restart's `warm_from_db`."""
        bucket = self._bucketer.get_bucket(edge.semantic_primitive)
        self._hot.setdefault(bucket, []).append(edge)
        if edge.embedding is not None:
            self._vec.add(edge.id, edge.embedding)
            self._warm_ids.add(edge.id)
