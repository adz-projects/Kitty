import uuid
import numpy as np
from ..types import BlendedHint, Hint

def find_blendable_pairs(edges, min_confidence=0.6, require_shared_domain=True,
                         max_blends=2):
    eligible = [e for e in edges if e.confidence >= min_confidence
                and e.tier in ("hot", "warm")]
    if len(eligible) < 2:
        return []
    pairs = []
    seen = set()
    for i, ea in enumerate(eligible):
        for j, eb in enumerate(eligible):
            if i >= j:
                continue
            if require_shared_domain:
                if ea.domain_id != eb.domain_id or not ea.domain_id:
                    continue
            pair_key = tuple(sorted([ea.id, eb.id]))
            if pair_key in seen:
                continue
            seen.add(pair_key)
            pairs.append((ea, eb))
    pairs.sort(key=lambda p: (p[0].confidence + p[1].confidence), reverse=True)
    return pairs[:max_blends]

def blend_hints(pairs):
    hints = []
    for ea, eb in pairs:
        text = f"Consider combining: {ea.semantic_primitive} + {eb.semantic_primitive}"
        avg_conf = (ea.confidence + eb.confidence) / 2.0
        hint = BlendedHint(
            text=text,
            confidence=round(avg_conf, 3),
            source_primitive_a=ea.semantic_primitive,
            source_primitive_b=eb.semantic_primitive,
            attribution_id=str(uuid.uuid4()),
            edge_id=None,
        )
        hints.append(hint)
    return hints
