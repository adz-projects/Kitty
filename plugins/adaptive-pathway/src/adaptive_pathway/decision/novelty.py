import mmh3
import numpy as np

class CountBasedNovelty:
    def __init__(self, config):
        nc = config["novelty"]
        self.n_tables = nc["n_hash_tables"]
        self.hash_size = nc["hash_size"]
        self.min_count_pessimistic = nc["min_count_pessimistic"]
        self.default_lambda = nc["default_lambda"]
        self.lambda_floor = nc.get("lambda_floor", 0.05)
        self.ucb_multiplier = nc.get("ucb_multiplier", 0.15)
        self._counts = [np.zeros(self.hash_size, dtype=np.int32) for _ in range(self.n_tables)]
        self._mode_config = config.get("mode", {})
        self._agent_multiplier = self._mode_config.get("agent_novelty_lambda_multiplier", 0.5)
        self._action_counts = {}
        self._user_exploration_score = 0.0
        self._user_exploration_weight = 0.1
        self._domain_lambdas = {}

    def _hash_embedding(self, embedding, table_idx):
        emb_bytes = np.asarray(embedding, dtype=np.float32).tobytes()
        return mmh3.hash(emb_bytes, seed=table_idx * 7919 + 7) % self.hash_size

    def bonus(self, context_embedding, lambda_override=None):
        lam = lambda_override if lambda_override is not None else self.default_lambda
        lam = max(lam, self.lambda_floor)
        if self.min_count_pessimistic:
            counts = [self._counts[t][self._hash_embedding(context_embedding, t)]
                      for t in range(self.n_tables)]
            min_count = min(counts)
        else:
            total = sum(self._counts[t][self._hash_embedding(context_embedding, t)]
                       for t in range(self.n_tables))
            min_count = total / self.n_tables
        return lam / (1.0 + float(min_count))

    def action_bonus(self, action_id, lambda_override=None):
        lam = lambda_override if lambda_override is not None else self.default_lambda
        lam = max(lam, self.lambda_floor)
        count = self._action_counts.get(action_id, 0)
        return self.ucb_multiplier * lam / (1.0 + float(count))

    def current_score(self, context_embedding):
        counts = [self._counts[t][self._hash_embedding(context_embedding, t)]
                  for t in range(self.n_tables)]
        mn = min(counts) if self.min_count_pessimistic else np.mean(counts)
        return 1.0 / (1.0 + float(mn))

    def visit(self, context_embedding):
        for t in range(self.n_tables):
            bucket = self._hash_embedding(context_embedding, t)
            self._counts[t][bucket] += 1

    def visit_action(self, action_id):
        self._action_counts[action_id] = self._action_counts.get(action_id, 0) + 1

    def visit_count(self, context_embedding):
        if self.min_count_pessimistic:
            return min(self._counts[t][self._hash_embedding(context_embedding, t)]
                      for t in range(self.n_tables))
        total = sum(self._counts[t][self._hash_embedding(context_embedding, t)]
                    for t in range(self.n_tables))
        return total // self.n_tables

    def action_count(self, action_id):
        return self._action_counts.get(action_id, 0)

    def record_user_action(self, action_id):
        self._user_exploration_score = (
            (1 - self._user_exploration_weight) * self._user_exploration_score +
            self._user_exploration_weight * 1.0
        )

    @property
    def user_exploration_active(self):
        return self._user_exploration_score > 0.5

    @property
    def user_exploration_score(self):
        return self._user_exploration_score

    def get_lambda_for_mode(self, mode):
        if mode == "agent":
            return max(self.default_lambda * self._agent_multiplier, self.lambda_floor)
        return self.default_lambda

    def get_lambda_for_domain(self, domain_id):
        return self._domain_lambdas.get(domain_id, self.default_lambda)

    def bump_domain_lambda(self, domain_id, amount):
        current = self.get_lambda_for_domain(domain_id)
        self._domain_lambdas[domain_id] = min(current + amount, self.default_lambda * 2.0)