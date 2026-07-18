import numpy as np
import yaml
from pathlib import Path
from src.adaptive_pathway.decision.in_session import InSessionBandit


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def test_initialization():
    config = _load_config()
    bandit = InSessionBandit(config)
    assert bandit.call_count == 0
    assert bandit.mix_weight == 0.0
    assert len(bandit.update_buffer) == 0


def test_mix_weight_grows():
    config = _load_config()
    bandit = InSessionBandit(config)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    for i in range(10):
        bandit.update(0, ctx, 1.0)
    assert 0.0 < bandit.mix_weight < 1.0


def test_mix_weight_capped():
    config = _load_config()
    bandit = InSessionBandit(config)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    for i in range(50):
        bandit.update(0, ctx, 1.0)
    assert bandit.mix_weight <= bandit.max_weight


def test_sample_returns_float():
    config = _load_config()
    bandit = InSessionBandit(config)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    score = bandit.sample(0, ctx)
    assert isinstance(score, float)


def test_update_affects_sample():
    config = _load_config()
    bandit = InSessionBandit(config)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    score_before = bandit.sample(0, ctx)
    bandit.update(0, ctx, 1.0)
    score_after = bandit.sample(0, ctx)
    assert score_before != score_after


def test_buffer_limits():
    config = _load_config()
    bandit = InSessionBandit(config)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    for i in range(30):
        bandit.update(i % 5, ctx, np.random.randn())
    assert len(bandit.update_buffer) <= bandit.buffer_size


def test_reset():
    config = _load_config()
    bandit = InSessionBandit(config)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    bandit.update(0, ctx, 1.0)
    bandit.update(1, ctx, -1.0)
    assert bandit.call_count == 2
    bandit.reset()
    assert bandit.call_count == 0
    assert len(bandit.update_buffer) == 0
    assert bandit.mix_weight == 0.0


def test_confidence_gating():
    config = _load_config()
    bandit = InSessionBandit(config)
    assert bandit.confidence_gate is True
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    score = bandit.sample(0, ctx)
    assert isinstance(score, float)
