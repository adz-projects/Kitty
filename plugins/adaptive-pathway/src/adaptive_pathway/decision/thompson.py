import numpy as np

class ThompsonLinUCB:
    def __init__(self, n_actions=20, d_features=64, noise_var=1.0):
        self.n_actions = n_actions
        self.d_features = d_features
        self.noise_var = noise_var
        self.A_inv = [np.eye(d_features, dtype=np.float64) for _ in range(n_actions)]
        self.b = [np.zeros(d_features, dtype=np.float64) for _ in range(n_actions)]
        # Cached Cholesky factor of `noise_var * A_inv[i]`, keyed by action
        # id, `None` when stale/uncomputed. `sample()` is called far more
        # often than `update()` (many `decide()` calls typically land between
        # two `record_outcome`s for the same action), so recomputing an
        # *identical* factorization on every single sample — decide()'s
        # actual hot path — dominated its latency (see
        # test_decide_latency_populated_graph, ~330ms -> ~30ms from the
        # SVD->Cholesky switch alone, still 3x over the 10ms budget without
        # this cache). Invalidated in `update`/`set_state`, the only places
        # that change `A_inv`.
        self._chol_cache = [None] * n_actions

    def sample(self, action_id, context):
        A_inv = self.A_inv[action_id]
        theta_hat = A_inv @ self.b[action_id]
        # A_inv is SPD by construction (starts at identity; Sherman-Morrison
        # downdates preserve positive-definiteness), so Cholesky-based
        # sampling is mathematically equivalent to numpy's default SVD-based
        # multivariate_normal but much faster — decide() must stay sub-10ms.
        # Fall back to the robust (slower) SVD path if numerical drift ever
        # makes `cov` non-PSD.
        L = self._chol_cache[action_id]
        if L is None:
            cov = self.noise_var * A_inv
            try:
                L = np.linalg.cholesky(cov)
            except np.linalg.LinAlgError:
                theta_sample = np.random.multivariate_normal(theta_hat, cov)
                return float(theta_sample @ context)
            self._chol_cache[action_id] = L
        z = np.random.standard_normal(self.d_features)
        theta_sample = theta_hat + L @ z
        return float(theta_sample @ context)

    def predict(self, action_id, context):
        A_inv = self.A_inv[action_id]
        theta_hat = A_inv @ self.b[action_id]
        mu = float(theta_hat @ context)
        sigma = float(np.sqrt(self.noise_var * context @ A_inv @ context))
        return mu, sigma

    def update(self, action_id, context, reward):
        A_inv = self.A_inv[action_id]
        v = A_inv @ context
        self.A_inv[action_id] = A_inv - np.outer(v, v) / (1.0 + float(context @ v))
        self.b[action_id] += reward * context
        self._chol_cache[action_id] = None

    def get_state(self, action_id):
        return {"A_inv": self.A_inv[action_id].tolist(), "b": self.b[action_id].tolist()}

    def set_state(self, action_id, state):
        self.A_inv[action_id] = np.array(state["A_inv"], dtype=np.float64)
        self.b[action_id] = np.array(state["b"], dtype=np.float64)
        self._chol_cache[action_id] = None
