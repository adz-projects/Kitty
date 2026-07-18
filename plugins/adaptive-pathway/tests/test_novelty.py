import numpy as np
from src.adaptive_pathway.decision.novelty import CountBasedNovelty
import yaml
from pathlib import Path


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def test_novelty_initialization():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    assert novelty.n_tables == 3
    assert novelty.hash_size == 2048
    assert novelty.min_count_pessimistic is True
    assert novelty.default_lambda == 0.5


def test_bonus_decays_with_visits():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    bonus_initial = novelty.bonus(ctx)
    assert bonus_initial > 0.0
    assert bonus_initial <= 1.0

    for _ in range(10):
        novelty.visit(ctx)

    bonus_after = novelty.bonus(ctx)
    assert bonus_after < bonus_initial


def test_current_score_matches_bonus_zero_visits():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    score = novelty.current_score(ctx)
    bonus = novelty.bonus(ctx)
    assert 0.0 < score <= 1.0
    assert 0.0 < bonus <= 0.5
    assert bonus == score * novelty.default_lambda


def test_visit_count_increments():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    assert novelty.visit_count(ctx) == 0
    novelty.visit(ctx)
    assert novelty.visit_count(ctx) == 1
    novelty.visit(ctx)
    assert novelty.visit_count(ctx) == 2


def test_min_count_pessimistic():
    config = _load_config()
    config["novelty"]["min_count_pessimistic"] = False
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    assert novelty.min_count_pessimistic is False
    for _ in range(5):
        novelty.visit(ctx)
    count = novelty.visit_count(ctx)
    assert count >= 0


def test_different_contexts_different_buckets():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx_a = np.ones(384, dtype=np.float32)
    ctx_b = -np.ones(384, dtype=np.float32)

    novelty.visit(ctx_a)
    novelty.visit(ctx_a)
    count_a = novelty.visit_count(ctx_a)
    count_b = novelty.visit_count(ctx_b)
    assert count_a > 0
    assert count_b == 0


def test_lambda_override():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    default_bonus = novelty.bonus(ctx)
    overridden_bonus = novelty.bonus(ctx, lambda_override=2.0)
    assert overridden_bonus > default_bonus


def test_get_lambda_for_mode():
    config = _load_config()
    novelty = CountBasedNovelty(config)

    tp_lam = novelty.get_lambda_for_mode("thought_partner")
    agent_lam = novelty.get_lambda_for_mode("agent")
    assert tp_lam == 0.5
    assert agent_lam < tp_lam


def test_hash_determinism():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    h1 = novelty._hash_embedding(ctx, 0)
    h2 = novelty._hash_embedding(ctx, 0)
    assert h1 == h2


def test_hash_seeds_produce_different_buckets():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    h0 = novelty._hash_embedding(ctx, 0)
    h1 = novelty._hash_embedding(ctx, 1)
    h2 = novelty._hash_embedding(ctx, 2)
    assert h0 != h1 or h1 != h2


def test_visit_count_after_many_visits():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    for _ in range(100):
        novelty.visit(ctx)

    count = novelty.visit_count(ctx)
    assert count == 100


def test_bonus_approaches_zero():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    for _ in range(1000):
        novelty.visit(ctx)

    bonus = novelty.bonus(ctx)
    assert bonus < 0.1


def test_lambda_floor_enforced():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    ctx = np.random.randn(384).astype(np.float32)

    bonus = novelty.bonus(ctx, lambda_override=0.0)
    assert bonus >= novelty.lambda_floor


def test_action_bonus_decays():
    config = _load_config()
    novelty = CountBasedNovelty(config)

    b1 = novelty.action_bonus("tool_x")
    novelty.visit_action("tool_x")
    b2 = novelty.action_bonus("tool_x")
    assert b2 < b1


def test_action_count_tracks():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    assert novelty.action_count("tool_x") == 0
    novelty.visit_action("tool_x")
    assert novelty.action_count("tool_x") == 1
    novelty.visit_action("tool_x")
    assert novelty.action_count("tool_x") == 2


def test_user_exploration_score_rises():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    assert novelty.user_exploration_score == 0.0
    assert novelty.user_exploration_active is False
    for _ in range(10):
        novelty.record_user_action("tool_x")
    assert novelty.user_exploration_score > 0.5
    assert novelty.user_exploration_active is True


def test_domain_lambda_bump():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    assert novelty.get_lambda_for_domain("python") == 0.5
    novelty.bump_domain_lambda("python", 0.02)
    assert novelty.get_lambda_for_domain("python") > 0.5
    for _ in range(100):
        novelty.bump_domain_lambda("python", 0.02)
    assert novelty.get_lambda_for_domain("python") <= 1.0


def test_get_lambda_for_mode_respects_floor():
    config = _load_config()
    novelty = CountBasedNovelty(config)
    lam = novelty.get_lambda_for_mode("agent")
    assert lam >= novelty.lambda_floor
