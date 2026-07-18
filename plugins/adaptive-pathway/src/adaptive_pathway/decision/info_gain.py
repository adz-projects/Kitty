import numpy as np
from .thompson import ThompsonLinUCB

class InformationGainModel(ThompsonLinUCB):
    def ig_score(self, action_id, context):
        A_inv = self.A_inv[action_id]
        sigma_before = float(np.sqrt(self.noise_var * context @ A_inv @ context))
        v = A_inv @ context
        denom = 1.0 + float(context @ v)
        sigma_after = float(np.sqrt(max(0, self.noise_var *
                           (context @ A_inv @ context - (context @ v) ** 2 / denom))))
        return sigma_before - sigma_after

    def sample(self, action_id, context):
        raw = self.ig_score(action_id, context)
        return 1.0 / (1.0 + np.exp(-raw * 5.0))

    def update(self, action_id, context, reward):
        super().update(action_id, context, reward)
