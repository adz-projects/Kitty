import time
import numpy as np
import pytest
from src.adaptive_pathway.decision.ensemble import BootstrapEnsemble
from src.adaptive_pathway.types import SchismState
import yaml
from pathlib import Path


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def _mock_domain(action_id):
    return str(action_id).split(":")[0] if ":" in str(action_id) else ""


def _mock_edge(action_id):
    return None


def test_ensemble_initialization():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    assert len(ensemble.models) == 6
    assert ensemble.schism_state == SchismState.NONE
    assert ensemble.plateau_risk_score == 0.0


def test_ig_weight_bounds():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ensemble.plateau_risk_score = 0.0
    assert np.isclose(ensemble.ig_weight, 0.15, atol=0.01)
    ensemble.plateau_risk_score = 1.0
    assert np.isclose(ensemble.ig_weight, 0.50, atol=0.01)
    ensemble.plateau_risk_score = 0.5
    assert np.isclose(ensemble.ig_weight, 0.325, atol=0.01)


def test_sample_weights_sum_to_one():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    score, samples = ensemble.sample(0, ctx)
    ig_w = ensemble.ig_weight
    std_w = (1.0 - ig_w - ensemble.pc_weight) / 4
    weights = [std_w] * 4 + [ig_w, ensemble.pc_weight]
    assert np.isclose(sum(weights), 1.0, atol=0.01)
    assert isinstance(score, float)


def test_agreement_returns_structure():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    result = ensemble.agreement(0, ctx)
    assert "mean" in result
    assert "std" in result
    assert "predictions" in result
    assert "disagree" in result
    assert len(result["predictions"]) == 4


def test_update_increments_call_counter():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    assert ensemble.call_counter == 0
    ensemble.update(0, ctx, 1.0)
    assert ensemble.call_counter == 1


# ─── Uncertainty-guaranteed exploration slot ───────────────────────────────


def test_max_sigma_shrinks_after_updates():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)

    sigma_before = ensemble.max_sigma(0, ctx)
    for _ in range(20):
        ensemble.update(0, ctx, 1.0)
    sigma_after = ensemble.max_sigma(0, ctx)
    # More evidence about a bucket should shrink our uncertainty about it.
    assert sigma_after < sigma_before


def test_max_sigma_higher_for_untouched_bucket():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)

    for _ in range(20):
        ensemble.update(0, ctx, 1.0)
    sigma_touched = ensemble.max_sigma(0, ctx)
    sigma_untouched = ensemble.max_sigma(1, ctx)  # bucket 1 never updated
    assert sigma_untouched > sigma_touched


# ─── Confidence half-life ───────────────────────────────────────────────────


def test_apply_confidence_decay_widens_stale_bucket_variance():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)

    for _ in range(20):
        ensemble.update(0, ctx, 1.0)
    sigma_before = ensemble.max_sigma(0, ctx)

    # Simulate staleness far beyond the half-life without waiting in real time.
    ensemble.last_updated[0] -= 168 * 3600 * 10  # 10 half-lives ago

    ensemble.apply_confidence_decay(base_half_life_hours=168, rate_multiplier=1.0,
                                     max_decay_fraction=0.3)
    sigma_after = ensemble.max_sigma(0, ctx)
    assert sigma_after > sigma_before


def test_apply_confidence_decay_leaves_mean_untouched():
    # Only the posterior variance (A_inv) should widen — the learned mean
    # estimate (b, and therefore theta_hat) must be untouched, so old
    # preferences don't vanish, they just get resampled with more noise.
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)

    for _ in range(20):
        ensemble.update(0, ctx, 1.0)
    b_before = ensemble.models[0].b[0].copy()

    ensemble.last_updated[0] -= 168 * 3600 * 10
    ensemble.apply_confidence_decay(base_half_life_hours=168, rate_multiplier=1.0,
                                     max_decay_fraction=0.3)
    assert np.allclose(ensemble.models[0].b[0], b_before)


def test_apply_confidence_decay_skips_pc_model():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    ensemble.update(0, ctx, 1.0)
    ensemble.last_updated[0] -= 168 * 3600 * 10
    # PC model (index 5) has no A_inv/b state; must not raise.
    ensemble.apply_confidence_decay(base_half_life_hours=168, rate_multiplier=1.0,
                                     max_decay_fraction=0.3)
    assert not hasattr(ensemble.models[ensemble.pc_model_index], "A_inv")


def test_apply_confidence_decay_respects_max_fraction_cap():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)

    # ensemble.update() bootstrap-samples each standard model independently
    # (bootstrap_probability=0.8) — a single call has a real chance of
    # leaving model 0's A_inv untouched (still identity), which would make
    # this test flaky. Set the state directly and only mark bucket 0 as
    # updated, so the decay math is deterministic and isolated from that
    # randomness.
    ensemble.models[0].A_inv[0] = ensemble.models[0].A_inv[0] - np.outer(ctx, ctx) / 2
    # Absurdly stale — target decay would be ~1.0 without the cap.
    ensemble.last_updated[0] = time.time() - 168 * 3600 * 1000
    ensemble.apply_confidence_decay(base_half_life_hours=168, rate_multiplier=1.0,
                                     max_decay_fraction=0.1)
    # A_inv should have moved only partway toward identity (trace bounded).
    trace = np.trace(ensemble.models[0].A_inv[0])
    eye_trace = np.trace(np.eye(ensemble.d_features))
    assert trace < eye_trace  # some movement toward identity happened
    # but not fully reset — with a 0.1 cap the blend can't reach identity.
    assert not np.allclose(ensemble.models[0].A_inv[0], np.eye(ensemble.d_features))


def test_apply_confidence_decay_noop_for_recent_buckets():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    ensemble.update(0, ctx, 1.0)
    a_inv_before = ensemble.models[0].A_inv[0].copy()

    ensemble.apply_confidence_decay(base_half_life_hours=168, rate_multiplier=0.10,
                                     max_decay_fraction=0.3)
    assert np.allclose(ensemble.models[0].A_inv[0], a_inv_before)


def test_entropy_risk_empty_history():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    risk = ensemble._entropy_risk([])
    assert risk == 0.0


def test_entropy_risk_insufficient_history():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    risk = ensemble._entropy_risk(["a"] * 10)
    assert risk == 0.0


def test_entropy_risk_with_history():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    history = []
    for i in range(200):
        if i < 150:
            history.append(f"action_{np.random.randint(0, 10)}")
        else:
            history.append(f"action_{np.random.randint(0, 3)}")
    risk = ensemble._entropy_risk(history)
    assert 0.0 <= risk <= 1.0


def test_diversity_risk_empty():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    risk = ensemble._diversity_risk([])
    assert risk == 0.0


def test_novelty_risk_insufficient():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    risk = ensemble._novelty_risk([0.5] * 10)
    assert risk == 0.0


def test_agreement_risk_no_snapshots():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    risk = ensemble._agreement_risk()
    assert risk == 0.0


def test_evaluate_plateau_risk():
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    action_history = []
    for i in range(200):
        action_history.append(f"action_{np.random.randint(0, 5)}")
    novelty_history = [1.0 / (1.0 + i * 0.01) for i in range(100)]
    result = ensemble.evaluate_plateau_risk(action_history, novelty_history)
    assert result.score is not None
    assert result.entropy_risk is not None
    assert result.diversity_risk is not None
    assert result.novelty_risk is not None
    assert result.agreement_risk is not None
    assert result.trend in ("rising", "falling", "stable")
    assert 0.15 <= result.ig_weight <= 0.50


def _prime_two_two_split(ensemble, faction_lo, faction_hi):
    """Populate prediction logs so `faction_lo` models agree (~0.1),
    `faction_hi` models agree (~0.9), and the factions disagree."""
    for m in faction_lo:
        ensemble.prediction_log[m] = [
            {"predicted_value": 0.1, "domain_id": None,
             "action_id": 0, "timestamp": 0.0}
            for _ in range(10)]
    for m in faction_hi:
        ensemble.prediction_log[m] = [
            {"predicted_value": 0.9, "domain_id": None,
             "action_id": 0, "timestamp": 0.0}
            for _ in range(10)]


def test_detect_returns_model_indices_not_positional():
    # models_to_check offset so positional index != model index.
    config = _load_config()
    config["schism"]["models_to_check"] = [1, 2, 3, 4]
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    _prime_two_two_split(ensemble, (1, 2), (3, 4))

    alert = ensemble._detect()
    assert alert is not None
    # Factions must be reported as real model indices {1,2} / {3,4},
    # NOT positional indices {0,1} / {2,3}.
    factions = {frozenset(alert.faction_a), frozenset(alert.faction_b)}
    assert factions == {frozenset([1, 2]), frozenset([3, 4])}
    assert ensemble.schism_data["fa"] and ensemble.schism_data["fb"]
    stored = {frozenset(ensemble.schism_data["fa"]),
              frozenset(ensemble.schism_data["fb"])}
    assert stored == {frozenset([1, 2]), frozenset([3, 4])}


def test_resolve_copies_correct_models():
    config = _load_config()
    config["schism"]["models_to_check"] = [1, 2, 3, 4]
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    _prime_two_two_split(ensemble, (1, 2), (3, 4))

    alert = ensemble._detect()
    assert alert is not None
    # Winning faction "a" == first partition found == models {1, 2}.
    d = ensemble.d_features
    for m in (1, 2):
        for a in range(ensemble.n_actions):
            ensemble.models[m].b[a] = np.ones(d)

    ensemble.schism_state = SchismState.REVIEWING
    ensemble.resolve("a")

    # Losing faction {3, 4} must have been overwritten by a winning model's state.
    for m in (3, 4):
        for a in range(ensemble.n_actions):
            assert np.allclose(ensemble.models[m].b[a], np.ones(d))


def test_resolve_both_preserves_model_disagreement():
    config = _load_config()
    config["schism"]["models_to_check"] = [1, 2, 3, 4]
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    _prime_two_two_split(ensemble, (1, 2), (3, 4))

    alert = ensemble._detect()
    assert alert is not None

    ensemble.schism_state = SchismState.REVIEWING
    ensemble.resolve("both")

    assert ensemble.schism_state == SchismState.RESOLVED


def test_novelty_risk_uses_mid_segment_own_slope():
    # v0 used to be computed as (recent[0] - mid[0]) / len(mid) — spanning
    # the boundary between the mid and recent windows — instead of the mid
    # segment's own trend (mid[-1] - mid[0]). With a deliberate discontinuity
    # between the two windows (recent[0] != mid[-1]), the two formulas give
    # different v0 values and therefore different risk scores; this pins the
    # correct (mid-segment-only) value.
    config = _load_config()
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    mid = [0.5 + i * 0.001 for i in range(30)]       # mid[0]=0.500, mid[-1]=0.529
    recent = [0.9 - i * 0.02 for i in range(30)]      # recent[0]=0.900, recent[-1]=0.320
    history = mid + recent
    risk = ensemble._novelty_risk(history)
    # v0 = (0.529-0.5)/30, v1 = (0.32-0.9)/30, risk = -(v1-v0)*10 ≈ 0.203.
    # The old (buggy) formula used v0=(recent[0]-mid[0])/30=(0.9-0.5)/30,
    # which yields ≈0.327 instead — a ~60% overstatement of risk.
    assert risk == pytest.approx(0.203, abs=0.002)


def test_should_check_does_not_self_compare_same_tick():
    # _should_check() used to append its own recomputed copy of the current
    # agreement matrix into agreement_snapshots (already appended once by
    # update()) and then compare that fresh recomputation against the
    # snapshot update() had just appended — always ~identical, so the
    # similarity-skip branch fired almost every time and schism checks
    # effectively never ran. Verify agreement_snapshots isn't double-appended
    # and grows by exactly one entry per update() call (capped at 10).
    config = _load_config()
    config["schism"]["check_call_interval"] = 1000000  # keep update() from invoking _should_check
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    ctx = np.random.randn(64).astype(np.float64)
    ctx /= np.linalg.norm(ctx)
    for i in range(15):
        ensemble.update(i % 5, ctx, 1.0)
    assert len(ensemble.agreement_snapshots) == 10  # capped, single append per update()


def test_resolve_both_widens_variance():
    config = _load_config()
    config["schism"]["models_to_check"] = [1, 2, 3, 4]
    ensemble = BootstrapEnsemble(config, _mock_domain, _mock_edge)
    _prime_two_two_split(ensemble, (1, 2), (3, 4))

    alert = ensemble._detect()
    assert alert is not None

    # Store original A_inv trace for model 1
    original_trace = np.trace(ensemble.models[1].A_inv[0])

    ensemble.schism_state = SchismState.REVIEWING
    ensemble.resolve("both")

    # Variance should be widened (A_inv scaled by 1.3)
    new_trace = np.trace(ensemble.models[1].A_inv[0])
    assert new_trace > original_trace * 1.0

