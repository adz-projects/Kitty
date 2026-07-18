import numpy as np
from src.adaptive_pathway.discovery.primitives import PrimitiveDiscoverer
from src.adaptive_pathway.discovery.domains import DomainDiscovery
from src.adaptive_pathway.discovery.centroids import CentroidManager
from src.adaptive_pathway.types import DomainSource, DetectionMethod
from src.adaptive_pathway.features import ActionBucketer
import yaml
from pathlib import Path


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def _mock_get_edges(action_ids):
    return []


def _mock_get_edge(edge_id):
    return None


class FakeEdge:
    def __init__(self, primitive, domain="", confidence=0.5, tier="hot",
                 domain_id=""):
        self.semantic_primitive = primitive
        self.domain = domain
        self.domain_id = domain_id or domain
        self.confidence = confidence
        self.tier = tier
        self.embedding = None
        self.id = f"edge_{primitive}"
        self.frequency = 1
        self.override_rate = 0.0
        self.co_selected_with = []
        self.status = "provisional"
        self.source = "auto_named"
        self.tags = []
        self.domain_tags = []
        self.last_accessed = ""
        self.created_at = ""


def test_primitive_discoverer_init():
    config = _load_config()
    bucketer = ActionBucketer(20)
    discoverer = PrimitiveDiscoverer(config, _mock_get_edges, bucketer)
    assert discoverer.call_interval == config["discovery"]["primitive_call_interval"]


def test_primitive_discovery_no_edges():
    config = _load_config()
    bucketer = ActionBucketer(20)
    discoverer = PrimitiveDiscoverer(config, _mock_get_edges, bucketer)
    discovered = discoverer.maybe_discover("s1", np.zeros(384), [])
    assert discovered == []


def test_primitive_discovery_with_edges():
    config = _load_config()
    bucketer = ActionBucketer(20)

    edges = [
        FakeEdge("python_write", domain="python"),
        FakeEdge("python_read", domain="python"),
        FakeEdge("js_fetch", domain="javascript"),
    ]

    def mock_get_edges(action_ids):
        return edges

    discoverer = PrimitiveDiscoverer(config, mock_get_edges, bucketer)
    discovered = discoverer.maybe_discover("s1", np.zeros(384), ["a1", "a2", "a3"])
    assert discovered == []

    for _ in range(48):
        discoverer.maybe_discover("s1", np.zeros(384), ["a1"])
    discovered = discoverer.maybe_discover("s1", np.zeros(384), ["a1"])
    assert len(discovered) == 3


def test_primitive_get_all():
    config = _load_config()
    bucketer = ActionBucketer(20)

    edges = [FakeEdge("tool_a"), FakeEdge("tool_b")]
    def mock_get_edges(action_ids):
        return edges

    discoverer = PrimitiveDiscoverer(config, mock_get_edges, bucketer)
    for _ in range(50):
        discoverer.maybe_discover("s1", np.zeros(384), ["a1"])
    primitives = discoverer.get_all_primitives()
    assert "tool_a" in primitives
    assert "tool_b" in primitives


def test_primitive_co_occurrence():
    config = _load_config()
    bucketer = ActionBucketer(20)

    edges = [FakeEdge("read"), FakeEdge("write"), FakeEdge("execute")]
    def mock_get_edges(action_ids):
        return edges

    discoverer = PrimitiveDiscoverer(config, mock_get_edges, bucketer)
    for _ in range(50):
        discoverer.maybe_discover("s1", np.zeros(384), ["a1"])
    co = discoverer.get_co_occurrence("read")
    assert isinstance(co, list)


def test_primitive_info():
    config = _load_config()
    bucketer = ActionBucketer(20)

    edges = [FakeEdge("known_tool")]
    def mock_get_edges(action_ids):
        return edges

    discoverer = PrimitiveDiscoverer(config, mock_get_edges, bucketer)
    for _ in range(50):
        discoverer.maybe_discover("s1", np.zeros(384), ["a1"])
    info = discoverer.get_primitive_info("known_tool")
    assert info is not None
    assert "source" in info
    info_none = discoverer.get_primitive_info("unknown")
    assert info_none is None


def test_domain_discovery_init():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    assert dd.domain_count == 0
    assert dd.max_domains == config["discovery"]["max_domains"]


def test_domain_add_and_get():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    ok = dd.add_domain("python", "Python Development")
    assert ok is True
    assert dd.domain_count == 1
    domain = dd.get_domain("python")
    assert domain["name"] == "Python Development"


def test_domain_max_limit():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    for i in range(10):
        dd.add_domain(f"domain_{i}", f"Domain {i}")
    assert dd.domain_count <= config["discovery"]["max_domains"]


def test_domain_lock_unlock():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    dd.add_domain("test", "Test Domain")
    dd.lock_domain("test")
    assert dd.get_domain("test")["locked"] is True
    dd.unlock_domain("test")
    assert dd.get_domain("test")["locked"] is False


def test_domain_list():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    dd.add_domain("py", "Python")
    dd.add_domain("js", "JavaScript")
    domains = dd.get_domains()
    assert len(domains) == 2
    assert domains[0]["id"] in ("py", "js")


def test_domain_update_centroid():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    dd.add_domain("py", "Python")
    emb = np.random.randn(384).astype(np.float32)
    emb /= np.linalg.norm(emb)
    dd.update_domain_centroid("py", emb)
    domain = dd.get_domain("py")
    assert domain.get("centroid") is not None


def test_domain_inference_no_data():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = dd.infer_domain(ctx, [], [])
    assert result is None


def test_domain_session_increment():
    config = _load_config()
    dd = DomainDiscovery(config, _mock_get_edge)
    dd.add_domain("py", "Python")
    dd.increment_session()
    assert dd._session_count == 1


def test_centroid_manager_init():
    config = _load_config()
    cm = CentroidManager(config)
    assert cm.ready is False
    assert cm.example_count == 0


def test_centroid_add_examples():
    config = _load_config()
    cm = CentroidManager(config)
    emb = np.random.randn(384).astype(np.float64)
    emb /= np.linalg.norm(emb)
    cm.add_example(emb, "keep_this")
    assert cm.example_count == 1
    cm.add_example(emb, "dont_do_again")
    assert cm.example_count == 2


def test_centroid_recompute():
    config = _load_config()
    config["preferences"]["centroid_min_examples"] = 5
    cm = CentroidManager(config)
    for _ in range(10):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "dont_do_again")
    for _ in range(5):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "keep_this")
    assert cm.ready is True


def test_centroid_classify():
    config = _load_config()
    config["preferences"]["centroid_min_examples"] = 5
    cm = CentroidManager(config)
    for _ in range(5):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "keep_this")
    for _ in range(10):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "dont_do_again")
    cm.recompute()
    test_emb = np.random.randn(384).astype(np.float64)
    test_emb /= np.linalg.norm(test_emb)
    result = cm.classify(test_emb)
    assert "type" in result
    assert "confidence" in result
    assert "method" in result


def test_centroid_not_ready_classify():
    config = _load_config()
    cm = CentroidManager(config)
    emb = np.random.randn(384).astype(np.float64)
    emb /= np.linalg.norm(emb)
    result = cm.classify(emb)
    assert result["type"] is None


def test_centroid_serialization():
    config = _load_config()
    config["preferences"]["centroid_min_examples"] = 5
    cm = CentroidManager(config)
    for _ in range(5):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "keep_this")
    for _ in range(10):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "dont_do_again")
    cm.recompute()
    data = cm.to_dict()
    assert data["ready"] is True
    assert data["positive_centroid"] is not None
    assert data["negative_centroid"] is not None
    assert data["example_count"] == 15

    cm2 = CentroidManager(config)
    cm2.from_dict(data)
    assert cm2.ready is True


def test_centroid_should_refresh():
    config = _load_config()
    config["preferences"]["centroid_min_examples"] = 5
    cm = CentroidManager(config)
    assert cm.should_refresh() is True
    for _ in range(5):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "keep_this")
    for _ in range(3):
        emb = np.random.randn(384).astype(np.float64)
        emb /= np.linalg.norm(emb)
        cm.add_example(emb, "dont_do_again")
    cm.recompute()
    assert cm.should_refresh() is False


def test_centroid_staleness():
    config = _load_config()
    cm = CentroidManager(config)
    assert cm.is_stale is False
