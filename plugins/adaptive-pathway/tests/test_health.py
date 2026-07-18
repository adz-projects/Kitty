import numpy as np
from src.adaptive_pathway.health import HealthChecker
from src.adaptive_pathway.features import FeatureHasher, ActionBucketer
from src.adaptive_pathway.types import SchismState, GraphHealth
import yaml
from pathlib import Path


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def _mock_get_state():
    return {"warm_ready": True, "sessions": 1}


def _mock_get_edges():
    return []


def _mock_get_novelty():
    return []


class FakeEnsemble:
    schism_state = SchismState.NONE
    plateau_risk_score = 0.0
    ig_weight = 0.15
    ensemble_diversity_mode = False


def _mock_get_ensemble():
    return FakeEnsemble()


def _mock_get_domains():
    return []


class FakeHasher:
    n_buckets = 64
    utilization = 0.3
    collision_rate = 0.05


class FakeDetector:
    centroids_ready = False
    _example_count = 0
    centroid_min_examples = 50


def test_health_checker_initialization():
    config = _load_config()
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    assert hc is not None


def test_run_full_check_returns_list():
    config = _load_config()
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    issues = hc.run_full_check()
    assert isinstance(issues, list)
    assert len(issues) >= 1


def test_get_graph_health():
    config = _load_config()
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    health = hc.get_graph_health()
    assert isinstance(health, GraphHealth)
    assert health.total_edges == 0
    assert "hot" in health.tier_distribution


def test_high_collision_detected():
    config = _load_config()
    hasher = FakeHasher()
    hasher.collision_rate = 0.25
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        hasher, FakeDetector(),
    )
    issues = hc._check_features()
    warnings = [i for i in issues if i.severity == "warning"]
    assert len(warnings) >= 1
    assert "collision" in warnings[0].message.lower()


def test_high_utilization_detected():
    config = _load_config()
    hasher = FakeHasher()
    hasher.utilization = 0.85
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        hasher, FakeDetector(),
    )
    issues = hc._check_features()
    assert len(issues) >= 1


def test_schism_detected():
    config = _load_config()
    ensemble = FakeEnsemble()
    ensemble.schism_state = SchismState.DETECTED
    def mock_ensemble():
        return ensemble
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, mock_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    issues = hc._check_ensemble()
    assert len(issues) >= 1
    assert "schism" in issues[0].message.lower()


def test_schism_none_no_warning():
    config = _load_config()
    ensemble = FakeEnsemble()
    ensemble.schism_state = SchismState.NONE
    def mock_ensemble():
        return ensemble
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, mock_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    issues = hc._check_ensemble()
    assert len(issues) == 0


def test_stale_novelty_detected():
    config = _load_config()
    novelty_vals = [0.01] * 100
    def mock_novelty():
        return novelty_vals
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        mock_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    issues = hc._check_novelty()
    assert len(issues) >= 1


def test_empty_graph_report():
    config = _load_config()
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    issues = hc._check_graph()
    info_messages = [i for i in issues if i.severity == "info"]
    assert len(info_messages) >= 1
    assert "empty" in info_messages[0].message.lower()


def test_preference_centroids_not_ready():
    config = _load_config()
    detector = FakeDetector()
    detector._example_count = 10
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), detector,
    )
    issues = hc._check_preferences()
    assert len(issues) >= 1


def test_override_rate_tracking():
    config = _load_config()
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    hc.update_metrics("action_a", 1.0)
    hc.update_metrics("action_b", -1.0)
    assert len(hc._override_history) == 2


def test_update_metrics_history_cap():
    config = _load_config()
    hc = HealthChecker(
        config, _mock_get_state, _mock_get_edges,
        _mock_get_novelty, _mock_get_ensemble, _mock_get_domains,
        FakeHasher(), FakeDetector(),
    )
    for i in range(150):
        hc.update_metrics(f"action_{i}", 0.5)
    assert len(hc._override_history) <= 100
