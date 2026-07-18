import numpy as np
import yaml
from pathlib import Path
from src.adaptive_pathway.decision.paradigm_challenge import ParadigmChallengeModel


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def _mock_get_domain(action_id):
    return str(action_id).split(":")[0] if ":" in str(action_id) else ""


def _mock_get_edge(action_id):
    class MockEdge:
        semantic_primitive = "default"
        co_selected_with = []
    return MockEdge()


def test_empty_graph_returns_zero():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)
    score = pc.sample(0, np.ones(64, dtype=np.float64))
    assert 0.0 <= score <= 0.1


def test_score_in_range():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)
    for _ in range(100):
        action_id = np.random.randint(0, 100)
        score = pc.sample(action_id, np.random.randn(64))
        assert 0.0 <= score <= 1.0


def test_predict_returns_tuple():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)
    result = pc.predict(0, np.ones(64))
    assert isinstance(result, tuple)
    assert len(result) == 2
    assert isinstance(result[0], float)
    assert result[1] == 0.0


def test_domain_isolation_signal():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)
    score = pc.score(
        "python:some_action", np.ones(64),
        top_n_action_ids=["js:action1", "js:action2"],
        domain_stats={"python": {"avg_confidence": 0.5, "avg_novelty": 0.0}},
    )
    assert score > 0.0


def test_domain_isolation_zero_when_domain_present():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)
    score_present = pc.score(
        "python:some_action", np.ones(64),
        top_n_action_ids=["python:action1", "python:action2"],
        domain_stats={"python": {"avg_confidence": 0.5, "avg_novelty": 0.0}},
    )
    score_absent = pc.score(
        "python:some_action", np.ones(64),
        top_n_action_ids=["js:action1", "js:action2"],
        domain_stats={"python": {"avg_confidence": 0.5, "avg_novelty": 0.0}},
    )
    assert score_absent > score_present


def test_confidence_gap_signal():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)

    score_low = pc.score(
        "python:action", np.ones(64),
        top_n_action_ids=["js:action1"],
        domain_stats={
            "python": {"avg_confidence": 0.3, "avg_novelty": 0.0},
            "js": {"avg_confidence": 0.9, "avg_novelty": 0.0},
        },
    )
    score_equal = pc.score(
        "python:action", np.ones(64),
        top_n_action_ids=["js:action1"],
        domain_stats={
            "python": {"avg_confidence": 0.9, "avg_novelty": 0.0},
            "js": {"avg_confidence": 0.9, "avg_novelty": 0.0},
        },
    )
    assert score_low > score_equal


def test_empty_domain_stats_handled():
    config = _load_config()
    pc = ParadigmChallengeModel(config, _mock_get_domain, _mock_get_edge)
    score = pc.score("action", np.ones(64), ["top1", "top2"], {})
    assert 0.0 <= score <= 1.0
