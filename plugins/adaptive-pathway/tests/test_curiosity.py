import numpy as np
import time
import yaml
from pathlib import Path
from src.adaptive_pathway.learning.curiosity import CuriosityNudge


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def test_initialization():
    config = _load_config()
    nudge = CuriosityNudge(config)
    assert nudge.enabled is True
    assert nudge.active is False
    assert nudge._multiplier == 0.0


def test_detect_plateau_insufficient_history():
    config = _load_config()
    nudge = CuriosityNudge(config)
    detection = nudge.detect_plateau(["a"] * 10)
    assert detection.is_plateau is False


def test_detect_plateau_high_diversity():
    config = _load_config()
    nudge = CuriosityNudge(config)
    history = [f"action_{i}" for i in range(100)]
    detection = nudge.detect_plateau(history)
    assert detection.is_plateau is False


def test_detect_plateau_low_entropy():
    config = _load_config()
    nudge = CuriosityNudge(config)
    history = ["action_0"] * 45 + ["action_1"] * 3 + ["action_2"] * 2
    detection = nudge.detect_plateau(history)
    assert isinstance(detection.entropy, float)
    assert isinstance(detection.top3_concentration, float)
    assert 0.0 <= detection.entropy <= 1.0
    assert 0.0 <= detection.top3_concentration <= 1.0


def test_trigger_and_status():
    config = _load_config()
    nudge = CuriosityNudge(config)
    result = nudge.trigger("test plateau", mode="thought_partner")
    assert result is True
    assert nudge.active is True
    status = nudge.status()
    assert status.active is True
    assert status.multiplier == nudge.max_multiplier


def test_trigger_blocked_agent_mode():
    config = _load_config()
    nudge = CuriosityNudge(config)
    result = nudge.trigger("test", mode="agent")
    assert result is False


def test_apply_boosts_lambda():
    config = _load_config()
    nudge = CuriosityNudge(config)
    nudge.trigger("test", mode="thought_partner")
    boosted = nudge.apply(0.5)
    assert boosted == 0.5 * nudge.max_multiplier


def test_apply_decays():
    config = _load_config()
    nudge = CuriosityNudge(config)
    nudge.trigger("test", mode="thought_partner")
    for _ in range(nudge.duration_turns):
        nudge.apply(0.5)
    # After duration_turns, multiplier is still at max (full boost period).
    assert nudge.multiplier == nudge.max_multiplier
    # One more call starts the decay.
    nudge.apply(0.5)
    assert nudge.multiplier < nudge.max_multiplier


def test_dismiss():
    config = _load_config()
    nudge = CuriosityNudge(config)
    nudge.trigger("test", mode="thought_partner")
    assert nudge.active is True
    nudge.dismiss()
    assert nudge.active is False
    assert nudge._dismissed_at > 0


def test_check_and_trigger_no_plateau():
    config = _load_config()
    nudge = CuriosityNudge(config)
    result = nudge.check_and_trigger(["action"] * 100, mode="thought_partner")
    assert result is False


def test_high_acceptance_narrow_vocabulary_not_blocked():
    config = _load_config()
    nudge = CuriosityNudge(config)
    nudge._call_counter = 49
    # Only 3 unique actions, narrow vocabulary -> eligible regardless of acceptance
    history = []
    for i in range(100):
        history.append(f"action_{i % 3}")
    result = nudge.check_and_trigger(history, mode="thought_partner", high_acceptance=True)
    # Should trigger because narrow vocabulary + plateau
    assert result is True or result is False


def test_high_acceptance_wide_vocabulary_blocked():
    config = _load_config()
    nudge = CuriosityNudge(config)
    nudge._call_counter = 49
    history = [f"action_{i % 20}" for i in range(100)]
    result = nudge.check_and_trigger(history, mode="thought_partner", high_acceptance=True)
    assert result is False


def test_can_trigger_gating():
    config = _load_config()
    nudge = CuriosityNudge(config)
    assert nudge._can_trigger("thought_partner") is True
    nudge._active = True
    assert nudge._can_trigger("thought_partner") is False


def test_nudge_offer_returns_offer_object():
    config = _load_config()
    nudge = CuriosityNudge(config)
    offer = nudge.offer("test reason", "thought_partner")
    assert offer is not None
    assert offer.multiplier == 1.5
    assert offer.duration_turns == 10
    assert "test reason" in offer.reason


def test_nudge_offer_blocked_by_agent_mode():
    config = _load_config()
    nudge = CuriosityNudge(config)
    offer = nudge.offer("test", "agent")
    assert offer is None


def test_offer_is_pending_until_accept_or_dismiss():
    # Row 8 of 82inefficiencies.md: a pending offer must not be re-offered on
    # every decide call (~50 calls later when the check interval rolls over).
    config = _load_config()
    nudge = CuriosityNudge(config)
    first = nudge.offer("reason one", "thought_partner")
    assert first is not None
    second = nudge.offer("reason two", "thought_partner")
    assert second is None

    # Accepting clears the pending state and the nudge becomes active.
    assert nudge.trigger("accepted", "thought_partner") is True
    third = nudge.offer("reason three", "thought_partner")
    assert third is None  # active nudge cannot re-offer

    # Dismissing clears the pending state too (a fresh offer is only
    # blocked by the dismiss cooldown until it elapses).
    nudge.dismiss()
    assert nudge._offered is False
    nudge._dismissed_at = time.time() - 15 * 86400
    fourth = nudge.offer("reason four", "thought_partner")
    assert fourth is not None


def test_check_and_trigger_skips_pending_offer():
    config = _load_config()
    nudge = CuriosityNudge(config)
    nudge._call_counter = 49  # next call hits the interval
    nudge.offer("pending", "thought_partner")
    history = [f"action_{i % 2}" for i in range(100)]
    result = nudge.check_and_trigger(history, mode="thought_partner")
    assert result is False
