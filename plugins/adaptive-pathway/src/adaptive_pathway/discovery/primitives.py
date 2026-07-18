import numpy as np
import time
from collections import Counter
from ..types import PrimitiveSource, EdgeInfo


class PrimitiveDiscoverer:
    def __init__(self, config, get_edges_fn, bucketer):
        dc = config["discovery"]
        self.call_interval = dc["primitive_call_interval"]
        self._get_edges = get_edges_fn
        self._bucketer = bucketer
        self._call_counter = 0
        self._discovered = {}
        self._co_occurrence = {}
        self._last_scan = 0.0

    def maybe_discover(self, session_id, context_embedding, available_actions):
        self._call_counter += 1
        if self._call_counter % self.call_interval != 0:
            return []
        edges = self._get_edges(available_actions)
        return self._extract_primitives(edges)

    def _extract_primitives(self, edges):
        discovered = []
        primitive_counts = Counter()
        for edge in edges:
            if edge.semantic_primitive:
                primitive_counts[edge.semantic_primitive] += 1
        for name, count in primitive_counts.most_common(10):
            if name not in self._discovered:
                self._discovered[name] = {
                    "source": PrimitiveSource.AUTO_NAMED,
                    "frequency": count,
                    "first_seen": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
                }
                discovered.append(name)
        for i, name_a in enumerate(list(primitive_counts.keys())[:20]):
            for name_b in list(primitive_counts.keys())[i + 1:20]:
                key = tuple(sorted([name_a, name_b]))
                self._co_occurrence[key] = self._co_occurrence.get(key, 0) + 1
        self._last_scan = time.time()
        return discovered

    def get_co_occurrence(self, primitive_name, top_k=5):
        related = []
        for (a, b), count in self._co_occurrence.items():
            if a == primitive_name or b == primitive_name:
                other = b if a == primitive_name else a
                related.append((other, count))
        related.sort(key=lambda x: x[1], reverse=True)
        return related[:top_k]

    def get_all_primitives(self):
        return list(self._discovered.keys())

    def get_primitive_info(self, name):
        return self._discovered.get(name)
