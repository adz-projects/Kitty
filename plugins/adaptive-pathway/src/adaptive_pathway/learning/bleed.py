import numpy as np


class DomainBleed:
    def __init__(self, config):
        bc = config["bleed"]
        self.default_temperature = bc["default_temperature"]
        self.domain_match_weight = bc["domain_match_weight"]

    def bleed_score(self, query_domain, edge_domain, temperature=None):
        temp = temperature if temperature is not None else self.default_temperature
        if not query_domain or not edge_domain:
            return 1.0
        if query_domain == edge_domain:
            return 1.0
        if temp <= 0.0:
            return 0.0
        return float(np.exp(-1.0 / temp))

    def domain_match_bonus(self, query_domain, edge_domain):
        if not query_domain or not edge_domain:
            return 1.0
        return self.domain_match_weight if query_domain == edge_domain else 1.0

    def rank_edges_by_domain(self, query_domain, edges, temperature=None):
        scored = []
        for edge in edges:
            edge_domain = getattr(edge, "domain_id", "") or getattr(edge, "domain", "")
            bleed = self.bleed_score(query_domain, edge_domain, temperature)
            match_bonus = self.domain_match_bonus(query_domain, edge_domain)
            scored.append((bleed * match_bonus, edge))
        scored.sort(key=lambda x: x[0], reverse=True)
        return scored
