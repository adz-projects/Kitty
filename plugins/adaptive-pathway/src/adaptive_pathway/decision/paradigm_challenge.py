import numpy as np

class ParadigmChallengeModel:
    def __init__(self, config, get_domain_fn, get_edge_fn):
        pc = config["paradigm_challenge"]
        self.top_n = pc["top_n"]
        self.w = pc["signal_weights"]
        self._get_domain = get_domain_fn
        self._get_edge = get_edge_fn

    def sample(self, action_id, context):
        return self.score(action_id, context, [], {})

    def score(self, action_id, context, top_n_action_ids, domain_stats):
        referent_domains = {self._get_domain(a) for a in top_n_action_ids} if top_n_action_ids else set()
        referent_edges = [self._get_edge(a) for a in top_n_action_ids]
        return self.score_with_referents(action_id, context, referent_domains,
                                         referent_edges, domain_stats)

    def score_with_referents(self, action_id, context, referent_domains,
                             referent_edges, domain_stats):
        # Referents (the candidate set defining isolation) are fixed for a
        # whole decide() call, but resolving them through the engine's
        # id-lookups costs O(edges) per referent per scored edge. Callers
        # with the candidate EdgeInfo objects already in hand precompute the
        # referent domains/edges once and pass them here.
        action_domain = self._get_domain(action_id)
        di = 1.0 if action_domain and action_domain not in referent_domains else 0.0

        dom_stat = domain_stats.get(action_domain, {})
        top_confs = [domain_stats.get(d, {}).get("avg_confidence", 0.5)
                     for d in referent_domains]
        avg_top = np.mean(top_confs) if top_confs else 0.5
        action_conf = dom_stat.get("avg_confidence", 0.5)
        cg = max(0.0, min(1.0, (avg_top - action_conf) * 2))

        action_edge = self._get_edge(action_id)
        cs_count = sum(1 for e in referent_edges if e and action_edge and
                       getattr(action_edge, "semantic_primitive", "") in
                       getattr(e, "co_selected_with", []))
        pi = 1.0 if cs_count == 0 and referent_edges else 0.0

        np_val = dom_stat.get("avg_novelty", 0.0)

        return min(1.0, self.w["domain_isolation"] * di +
                         self.w["confidence_gap"] * cg +
                         self.w["primitive_isolation"] * pi +
                         self.w["novelty_persistence"] * np_val)

    def predict(self, action_id, context):
        s = self.sample(action_id, context)
        return s, 0.0
