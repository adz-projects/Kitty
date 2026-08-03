import numpy as np
import time
from ..types import DomainSource


class DomainDiscovery:
    def __init__(self, config, get_edge_fn):
        dc = config["discovery"]
        self.session_interval = dc["domain_session_interval"]
        self.reinference_days = dc["domain_reinference_days"]
        self.override_rate_threshold = dc["domain_override_rate_threshold"]
        self.max_domains = dc["max_domains"]
        self._get_edge = get_edge_fn
        self._domains = {}
        self._call_counter = 0
        self._session_count = 0
        self._unassigned_pool = []

    @property
    def domain_count(self):
        return len(self._domains)

    def get_domains(self):
        return [
            {
                "id": did,
                "name": info["name"],
                "source": info.get("source", DomainSource.AUTO_NAMED).value
                if hasattr(info.get("source", DomainSource.AUTO_NAMED), "value")
                else str(info.get("source", "auto_named")),
                "confidence": info.get("confidence", 0.5),
                "edge_count": info.get("edge_count", 0),
                "last_inferred": info.get("last_inferred"),
                "locked": info.get("locked", False),
            }
            for did, info in self._domains.items()
        ]

    def get_domain(self, domain_id):
        return self._domains.get(domain_id)

    def add_domain(self, domain_id, name, source=DomainSource.AUTO_NAMED,
                   locked=False):
        if domain_id not in self._domains and len(self._domains) < self.max_domains:
            self._domains[domain_id] = {
                "name": name,
                "source": source,
                "confidence": 0.5,
                "edge_count": 0,
                "last_inferred": time.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "locked": locked,
            }
            return True
        return False

    def infer_domain(self, context_embedding, available_actions, edges):
        if len(self._domains) >= self.max_domains:
            return None
        ctx = np.asarray(context_embedding, dtype=np.float32).ravel()
        norm = float(np.linalg.norm(ctx))
        if norm < 1e-10:
            return None
        ctx = ctx / norm
        centroid_embeddings = {}
        for did, info in self._domains.items():
            centroid = info.get("centroid")
            if centroid is not None:
                centroid_embeddings[did] = np.asarray(centroid, dtype=np.float32).ravel()
        if centroid_embeddings:
            best_sim = 0.0
            best_domain = None
            for did, centroid in centroid_embeddings.items():
                c_norm = float(np.linalg.norm(centroid))
                if c_norm < 1e-10:
                    continue
                centroid = centroid / c_norm
                sim = float(np.dot(ctx, centroid))
                if sim > best_sim:
                    best_sim = sim
                    best_domain = did
            if best_sim >= 0.6 and best_domain:
                return best_domain
        if len(self._unassigned_pool) < 50:
            self._unassigned_pool.append(ctx)
        return None

    def update_domain_centroid(self, domain_id, embedding):
        if domain_id not in self._domains:
            return
        emb = np.asarray(embedding, dtype=np.float32).ravel()
        norm = float(np.linalg.norm(emb))
        if norm < 1e-10:
            return
        emb = emb / norm
        current = self._domains[domain_id].get("centroid")
        if current is None:
            self._domains[domain_id]["centroid"] = emb
        else:
            alpha = 0.05
            new_centroid = (1 - alpha) * np.asarray(current, dtype=np.float32) + alpha * emb
            n = float(np.linalg.norm(new_centroid))
            self._domains[domain_id]["centroid"] = new_centroid / n if n > 1e-10 else new_centroid

    def increment_session(self):
        self._session_count += 1
        if self._session_count % self.session_interval == 0:
            self._attempt_reinference()

    def _attempt_reinference(self):
        now = time.time()
        for did, info in list(self._domains.items()):
            if info.get("locked"):
                continue
            last = info.get("last_inferred_ts", 0)
            if now - last > self.reinference_days * 86400:
                info["needs_reinference"] = True

    def lock_domain(self, domain_id):
        if domain_id in self._domains:
            self._domains[domain_id]["locked"] = True

    def unlock_domain(self, domain_id):
        if domain_id in self._domains:
            self._domains[domain_id]["locked"] = False

    def estimate_centroids_from_pool(self):
        if len(self._unassigned_pool) < 10:
            return
        pool = np.stack(self._unassigned_pool[-100:])
        if pool.shape[0] < 10:
            return
        similarity = pool @ pool.T
        mean_sim = np.mean(similarity, axis=0)
        cluster_center = pool[int(np.argmax(mean_sim))]
        return cluster_center

    def clear_unassigned_pool(self):
        self._unassigned_pool = []

    def to_dict(self):
        """Serialize domain entries for persistence (row 3).
        Numpy centroids are converted to BLOB-ready byte strings."""
        out = {}
        for did, info in self._domains.items():
            entry = dict(info)
            centroid = info.get("centroid")
            if centroid is not None:
                entry["centroid"] = np.asarray(centroid, dtype=np.float32).tobytes()
            out[did] = entry
        return out

    def from_dict(self, domains):
        """Restore domain entries from persisted data (row 3).
        Replaces the current in-memory domain set."""
        self._domains.clear()
        for did, info in domains.items():
            entry = dict(info)
            centroid_raw = info.get("centroid")
            if centroid_raw is not None:
                raw = centroid_raw if isinstance(centroid_raw, (bytes, bytearray)) else centroid_raw
                if isinstance(raw, (bytes, bytearray)) and len(raw) > 0:
                    entry["centroid"] = np.frombuffer(raw, dtype=np.float32)
                elif isinstance(raw, list):
                    entry["centroid"] = np.asarray(raw, dtype=np.float32)
                else:
                    entry["centroid"] = None
            self._domains[did] = entry
