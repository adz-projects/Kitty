import numpy as np
from .thompson import ThompsonLinUCB

class InSessionBandit:
    def __init__(self, config):
        ic = config["in_session"]
        self.n_actions = config["thompson"]["max_action_buckets"]
        self.d_features = config["thompson"]["feature_buckets"]
        self.noise_var = ic["noise_variance"]
        self.max_weight = ic["max_mix_weight"]
        self.calls_to_max = ic["calls_to_max"]
        self.buffer_size = ic["buffer_size"]
        self.recency_half_life = ic["recency_half_life"]
        self.confidence_gate = ic["confidence_gate"]
        self.learn_during_pause = ic["learn_during_pause"]
        self.model = ThompsonLinUCB(self.n_actions, self.d_features, noise_var=self.noise_var)
        self.call_count = 0
        self.update_buffer = []

    @property
    def mix_weight(self):
        return min(self.max_weight, self.call_count / max(self.calls_to_max, 1))

    def sample(self, action_id, context):
        raw = self.model.sample(action_id, context)
        if not self.confidence_gate:
            return raw
        _, sigma = self.model.predict(action_id, context)
        max_sigma = float(np.sqrt(self.noise_var))
        if max_sigma < 1e-10:
            return raw
        return raw * (1.0 - min(1.0, sigma / max_sigma))

    def update(self, action_id, context, reward):
        self.call_count += 1
        self.update_buffer.append((action_id, context, reward, self.call_count))
        if len(self.update_buffer) > self.buffer_size:
            self.update_buffer.pop(0)
        self._rebuild_from_buffer()

    def _rebuild_from_buffer(self):
        self.model = ThompsonLinUCB(self.n_actions, self.d_features, noise_var=self.noise_var)
        for aid, ctx, r, call_idx in self.update_buffer:
            recency = np.exp(-np.log(2) * (self.call_count - call_idx) / max(self.recency_half_life, 1))
            self.model.update(aid, ctx, r * recency)

    def reset(self):
        self.model = ThompsonLinUCB(self.n_actions, self.d_features, noise_var=self.noise_var)
        self.call_count = 0
        self.update_buffer = []
