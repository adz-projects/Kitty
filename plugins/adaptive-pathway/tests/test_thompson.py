import numpy as np
from src.adaptive_pathway.decision.thompson import ThompsonLinUCB


def test_initialization():
    mab = ThompsonLinUCB(n_actions=20, d_features=64, noise_var=1.0)
    assert mab.n_actions == 20
    assert mab.d_features == 64
    assert len(mab.A_inv) == 20
    assert all(a.shape == (64, 64) for a in mab.A_inv)
    assert all(np.allclose(a, np.eye(64)) for a in mab.A_inv)
    assert len(mab.b) == 20
    assert all(b.shape == (64,) for b in mab.b)
    assert all(np.allclose(b, np.zeros(64)) for b in mab.b)


def test_sample_returns_float():
    mab = ThompsonLinUCB(n_actions=10, d_features=8)
    context = np.ones(8, dtype=np.float64)
    context /= np.linalg.norm(context)
    score = mab.sample(0, context)
    assert isinstance(score, float)


def test_predict_returns_mu_sigma():
    mab = ThompsonLinUCB(n_actions=10, d_features=8)
    context = np.random.randn(8)
    context /= np.linalg.norm(context)
    mu, sigma = mab.predict(3, context)
    assert isinstance(mu, float)
    assert isinstance(sigma, float)
    assert sigma >= 0


def test_update_changes_posterior():
    mab = ThompsonLinUCB(n_actions=10, d_features=4)
    context = np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float64)
    mu_before, _ = mab.predict(0, context)
    mab.update(0, context, 1.0)
    mu_after, _ = mab.predict(0, context)
    assert mu_after > mu_before


def test_negative_reward():
    mab = ThompsonLinUCB(n_actions=10, d_features=4)
    context = np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float64)
    mu_before, _ = mab.predict(0, context)
    mab.update(0, context, -1.0)
    mu_after, _ = mab.predict(0, context)
    assert mu_after < mu_before


def test_multiple_updates_converge():
    mab = ThompsonLinUCB(n_actions=5, d_features=3)
    true_theta = np.array([1.0, -0.5, 0.2])
    for _ in range(100):
        context = np.random.randn(3)
        context /= np.linalg.norm(context)
        reward = float(true_theta @ context) + np.random.normal(0, 0.1)
        mab.update(0, context, reward)
    theta_hat = mab.A_inv[0] @ mab.b[0]
    assert np.allclose(theta_hat, true_theta, atol=0.5)


def test_state_roundtrip():
    mab = ThompsonLinUCB(n_actions=5, d_features=3)
    context = np.array([1.0, 0.5, 0.0])
    context /= np.linalg.norm(context)
    mab.update(0, context, 1.0)
    state = mab.get_state(0)
    mab2 = ThompsonLinUCB(n_actions=5, d_features=3)
    mab2.set_state(0, state)
    mu1, sigma1 = mab.predict(0, context)
    mu2, sigma2 = mab2.predict(0, context)
    assert np.isclose(mu1, mu2)
    assert np.isclose(sigma1, sigma2)


def test_different_actions_independent():
    mab = ThompsonLinUCB(n_actions=5, d_features=3)
    ctx = np.array([1.0, 0.0, 0.0])
    ctx /= np.linalg.norm(ctx)
    mu0_before, _ = mab.predict(0, ctx)
    mu1_before, _ = mab.predict(1, ctx)
    mab.update(0, ctx, 1.0)
    mu0_after, _ = mab.predict(0, ctx)
    mu1_after, _ = mab.predict(1, ctx)
    assert mu0_after > mu0_before
    assert np.isclose(mu1_after, mu1_before)


def test_sherman_morrison_positive_definite():
    mab = ThompsonLinUCB(n_actions=5, d_features=3)
    for _ in range(20):
        ctx = np.random.randn(3)
        ctx /= np.linalg.norm(ctx)
        mab.update(0, ctx, np.random.randn())
    A_inv = mab.A_inv[0]
    A = np.linalg.inv(A_inv)
    eigenvalues = np.linalg.eigvalsh(A)
    assert np.all(eigenvalues > 0)
