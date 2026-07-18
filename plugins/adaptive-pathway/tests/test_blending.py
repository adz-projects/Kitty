from src.adaptive_pathway.decision.blending import find_blendable_pairs, blend_hints
from src.adaptive_pathway.types import EdgeInfo, BlendedHint, EdgeStatus, PrimitiveSource


def _make_edge(eid, primitive, domain_id, confidence, tier="hot"):
    return EdgeInfo(
        id=eid, semantic_primitive=primitive, domain_id=domain_id,
        domain=domain_id, confidence=confidence, tier=tier,
        status=EdgeStatus.ESTABLISHED,
        source=PrimitiveSource.AUTO_NAMED,
    )


def test_find_blendable_pairs_empty():
    pairs = find_blendable_pairs([], min_confidence=0.6)
    assert pairs == []


def test_find_blendable_pairs_single():
    edges = [_make_edge("a", "prim_a", "dom1", 0.8)]
    pairs = find_blendable_pairs(edges, min_confidence=0.6)
    assert pairs == []


def test_find_blendable_pairs_below_threshold():
    edges = [_make_edge("a", "prim_a", "dom1", 0.5), _make_edge("b", "prim_b", "dom1", 0.5)]
    pairs = find_blendable_pairs(edges, min_confidence=0.6)
    assert pairs == []


def test_find_blendable_pairs_different_domains():
    edges = [_make_edge("a", "prim_a", "dom1", 0.8), _make_edge("b", "prim_b", "dom2", 0.8)]
    pairs = find_blendable_pairs(edges, min_confidence=0.6, require_shared_domain=True)
    assert pairs == []


def test_find_blendable_pairs_success():
    edges = [_make_edge("a", "prim_a", "dom1", 0.8), _make_edge("b", "prim_b", "dom1", 0.9)]
    pairs = find_blendable_pairs(edges, min_confidence=0.6, require_shared_domain=True)
    assert len(pairs) == 1
    assert pairs[0][0].id == "a"
    assert pairs[0][1].id == "b"


def test_find_blendable_pairs_max_blends():
    edges = [
        _make_edge("a", "prim_a", "dom1", 0.9),
        _make_edge("b", "prim_b", "dom1", 0.9),
        _make_edge("c", "prim_c", "dom1", 0.9),
        _make_edge("d", "prim_d", "dom1", 0.9),
    ]
    pairs = find_blendable_pairs(edges, min_confidence=0.6, require_shared_domain=True, max_blends=2)
    assert len(pairs) == 2


def test_blend_hints_creates_correct_types():
    edges = [_make_edge("a", "prim_a", "dom1", 0.8), _make_edge("b", "prim_b", "dom1", 0.9)]
    hints = blend_hints([(edges[0], edges[1])])
    assert len(hints) == 1
    assert isinstance(hints[0], BlendedHint)
    assert hints[0].source_primitive_a == "prim_a"
    assert hints[0].source_primitive_b == "prim_b"


def test_blended_hint_fallback_properties():
    edges = [_make_edge("a", "prim_a", "dom1", 0.8), _make_edge("b", "prim_b", "dom1", 0.9)]
    hints = blend_hints([(edges[0], edges[1])])
    hint = hints[0]
    assert hint.primitive == "prim_a"
    assert hint.domain == ""
