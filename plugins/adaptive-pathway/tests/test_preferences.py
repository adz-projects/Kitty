import numpy as np
import yaml
from pathlib import Path

from adaptive_pathway.learning.preferences import PreferenceDetector, PreferenceIntensity
from adaptive_pathway.types import AnnotationType, DetectionMethod


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def _train_centroids(detector):
    rng = np.random.default_rng(1)
    pos_base = rng.standard_normal(384)
    pos_base /= np.linalg.norm(pos_base)
    neg_base = rng.standard_normal(384)
    neg_base /= np.linalg.norm(neg_base)
    for _ in range(60):
        p = pos_base + rng.standard_normal(384) * 0.01
        detector.add_labeled_example(p, "keep_this", intensity=0.6)
        n = neg_base + rng.standard_normal(384) * 0.01
        detector.add_labeled_example(n, "dont_do_again", intensity=0.6)
    assert detector.centroids_ready
    return pos_base, neg_base, rng


def test_uncertain_embedding_stages_for_confirmation():
    # stage_for_confirmation/tick_pending existed but were never invoked —
    # _detect_behavioral always returned None. An embedding that's plausible
    # (above embedding_uncertainty_threshold) but not confident enough to
    # commit (below embedding_confidence_threshold) should now get staged.
    config = _load_config()
    detector = PreferenceDetector(config)
    pos_base, neg_base, rng = _train_centroids(detector)

    # Mostly the negative centroid plus enough noise to land the similarity
    # in the uncertain band (~0.3-0.7) rather than confidently over 0.7.
    noise = rng.standard_normal(384)
    uncertain = neg_base + noise * 0.08
    uncertain /= np.linalg.norm(uncertain)
    pos_sim = float(np.dot(uncertain, detector._positive_centroid))
    neg_sim = float(np.dot(uncertain, detector._negative_centroid))
    assert pos_sim < detector.embedding_confidence_threshold
    assert detector.embedding_uncertainty_threshold <= neg_sim < detector.embedding_confidence_threshold

    assert len(detector._pending_confirmations) == 0
    result = detector.detect(uncertain)
    assert result["type"] is None  # not confident enough to commit yet
    assert len(detector._pending_confirmations) == 1
    staged = next(iter(detector._pending_confirmations.values()))
    assert staged["candidate_type"] == AnnotationType.DONT_DO_AGAIN


def test_pending_confirmation_is_resolved_and_cleared_after_wait_turns():
    # tick_pending() must actually count down, and once turns_remaining
    # hits zero, the next uncertain-band detect() call must reach
    # _detect_behavioral and clear the entry (whether or not that later
    # turn's embedding ends up confirming the candidate).
    config = _load_config()
    config["preferences"]["behavioral_confirmation_wait_turns"] = 1
    detector = PreferenceDetector(config)
    pos_base, neg_base, rng = _train_centroids(detector)

    detector.stage_for_confirmation(AnnotationType.DONT_DO_AGAIN, neg_base)
    assert len(detector._pending_confirmations) == 1
    original_key = next(iter(detector._pending_confirmations))

    detector.tick_pending()  # turns_remaining: 1 -> 0, now due for resolution

    noise = rng.standard_normal(384)
    uncertain = neg_base + noise * 0.08
    uncertain /= np.linalg.norm(uncertain)
    # detect() falls through embedding -> behavioral -> heuristic when each
    # returns type=None, so the final result reflects whichever detector
    # resolved it. This call's own uncertainty also re-stages a fresh
    # (not-yet-due) entry via _detect_embedding — the behaviorally-relevant
    # assertion is that the ORIGINAL due entry was processed and cleared by
    # _detect_behavioral, not that the dict ends up empty.
    detector.detect(uncertain)
    assert original_key not in detector._pending_confirmations


def test_tick_pending_purges_stale_entries():
    # tick_pending() used to decrement turns_remaining forever with no
    # upper bound on staleness, growing _pending_confirmations unboundedly
    # for embeddings that never got a matching record_annotation() call.
    config = _load_config()
    detector = PreferenceDetector(config)
    detector.stage_for_confirmation(AnnotationType.KEEP_THIS, np.ones(384))
    for _ in range(15):
        detector.tick_pending()
    assert len(detector._pending_confirmations) == 0
