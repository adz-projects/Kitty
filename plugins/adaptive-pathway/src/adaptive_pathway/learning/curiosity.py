import numpy as np
import time
from ..types import NudgeStatus, PlateauDetection, NudgeOffer

class CuriosityNudge:
    def __init__(self, config, novelty=None):
        cn = config["curiosity_nudge"]
        self.enabled = cn["enabled"]
        self.entropy_threshold = cn["entropy_threshold"]
        self.concentration_threshold = cn["concentration_threshold"]
        self.window_turns = cn["window_turns"]
        self.max_multiplier = cn["max_multiplier"]
        self.duration_turns = cn["duration_turns"]
        self.decay_rate = cn["decay_rate"]
        self.cooldown_days = cn["cooldown_dismissed_days"]
        self.check_interval = cn["check_interval"]
        self.agent_mode_allowed = cn["agent_mode_allowed"]
        self.high_acceptance_blocked = cn["high_acceptance_blocked"]
        self.high_acceptance_action_width = cn.get("high_acceptance_action_width", 6)
        self._novelty = novelty
        self._active = False
        self._multiplier = 0.0
        self._reason = ""
        self._turns_remaining = 0
        self._dismissed_at = 0.0
        self._call_counter = 0
        self._turns_since_trigger = 0
        self._offered = False

    @property
    def active(self):
        return self._active

    @property
    def multiplier(self):
        return self._multiplier

    def _can_trigger(self, mode):
        if not self.enabled:
            return False
        if not self.agent_mode_allowed and mode == "agent":
            return False
        if self.cooldown_days > 0 and self._dismissed_at > 0:
            days_since = (time.time() - self._dismissed_at) / 86400.0
            if days_since < self.cooldown_days:
                return False
        if self._active:
            return False
        if self._novelty and self._novelty.user_exploration_active:
            return False
        return True

    def detect_plateau(self, action_history):
        if len(action_history) < self.window_turns:
            return PlateauDetection(entropy=1.0, top3_concentration=0.0,
                                    is_plateau=False, dominant_actions=[])
        window = action_history[-self.window_turns:]
        counts = {}
        for a in window:
            counts[a] = counts.get(a, 0) + 1
        total = len(window)
        probs = [c / total for c in counts.values()]
        entropy = -sum(p * np.log(max(p, 1e-10)) for p in probs)
        n_unique = len(counts)
        if n_unique > 1:
            entropy = entropy / np.log(n_unique)
        sorted_actions = sorted(counts.items(), key=lambda x: x[1], reverse=True)
        top3_total = sum(c for _, c in sorted_actions[:3])
        concentration = top3_total / total
        is_plateau = bool(entropy < self.entropy_threshold and concentration > self.concentration_threshold)
        dominant = sorted_actions[:3]
        return PlateauDetection(
            entropy=round(float(entropy), 3),
            top3_concentration=round(float(concentration), 3),
            is_plateau=is_plateau,
            dominant_actions=dominant,
        )

    def offer(self, reason, mode="thought_partner"):
        if not self._can_trigger(mode):
            return None
        # Row 8 of 82inefficiencies.md: an offer is read state, not a
        # repeating reminder. While one is pending (not yet accepted via
        # `trigger` or dismissed), keep returning None so the same nudge
        # isn't re-offered on every decide call.
        if self._offered:
            return None
        self._offered = True
        return NudgeOffer(
            multiplier=self.max_multiplier,
            duration_turns=self.duration_turns,
            reason=reason,
        )

    def trigger(self, reason, mode="thought_partner"):
        if not self._can_trigger(mode):
            return False
        self._active = True
        self._multiplier = self.max_multiplier
        self._reason = reason
        self._turns_remaining = self.duration_turns
        self._turns_since_trigger = 0
        self._offered = False
        return True

    def apply(self, novelty_lambda):
        if not self._active:
            return novelty_lambda
        if self._turns_remaining > 0:
            self._turns_remaining -= 1
            self._turns_since_trigger += 1
            return novelty_lambda * self._multiplier
        self._multiplier -= self.decay_rate
        self._turns_since_trigger += 1
        if self._multiplier <= 1.0:
            self._active = False
            self._multiplier = 0.0
            return novelty_lambda
        return novelty_lambda * self._multiplier

    def dismiss(self):
        self._active = False
        self._multiplier = 0.0
        self._offered = False
        self._dismissed_at = time.time()

    def status(self):
        if not self._active:
            return None
        return NudgeStatus(
            active=self._active,
            multiplier=round(self._multiplier, 3),
            reason=self._reason,
            turns_remaining=self._turns_remaining,
        )

    def check_and_trigger(self, action_history, mode="thought_partner",
                          high_acceptance=False):
        self._call_counter += 1
        if self._call_counter % self.check_interval != 0:
            return False
        # Never auto-re-trigger while an offer is still pending — the offer
        # must persist until the user accepts or dismisses it.
        if self._offered:
            return False
        if self.high_acceptance_blocked and high_acceptance:
            unique_actions = len(set(action_history[-self.window_turns:]))
            if unique_actions >= self.high_acceptance_action_width:
                return False
        detection = self.detect_plateau(action_history)
        if detection.is_plateau:
            reason = f"Low entropy ({detection.entropy}), high concentration ({detection.top3_concentration})"
            if high_acceptance and self.high_acceptance_blocked:
                reason += f" — narrow vocabulary ({unique_actions} unique)"
            return self.trigger(reason=reason, mode=mode)
        return False