import time
from datetime import datetime, timedelta


class EdgeTTL:
    def __init__(self, config):
        tc = config["ttl"]
        self.auto_enabled = tc["auto_enabled"]
        self.permission_denied_hours = tc["permission_denied_hours"]
        self.deprecated_api_hours = tc["deprecated_api_hours"]
        self.syntax_crash_hours = tc["syntax_crash_hours"]
        self.user_dont_do_again_hours = tc.get("user_dont_do_again_hours", 720)
        self.user_override_clears_ttl = tc["user_override_clears_ttl"]
        self._ttl_store = {}

    def set_ttl(self, edge_id, cause):
        if not self.auto_enabled:
            return
        hours = {
            "permission_denied": self.permission_denied_hours,
            "deprecated_api": self.deprecated_api_hours,
            "syntax_crash": self.syntax_crash_hours,
            "user_rejected": self.user_dont_do_again_hours,
        }.get(cause, 24)
        expires = datetime.utcnow() + timedelta(hours=hours)
        self._ttl_store[edge_id] = {
            "expires_at": expires.isoformat(),
            "cause": cause,
            "set_at": datetime.utcnow().isoformat(),
        }

    def is_expired(self, edge_id):
        """Despite the name, this returns True while the edge's TTL is
        currently ACTIVE (i.e. the edge should be suppressed), and False
        once the TTL window has lapsed (at which point the entry is
        deleted). Callers use `not is_expired(...)` to mean "safe to use"."""
        if edge_id not in self._ttl_store:
            return False
        expires = datetime.fromisoformat(self._ttl_store[edge_id]["expires_at"])
        if datetime.utcnow() >= expires:
            del self._ttl_store[edge_id]
            return False
        return True

    def clear_ttl(self, edge_id):
        self._ttl_store.pop(edge_id, None)

    def get_ttl(self, edge_id):
        if edge_id not in self._ttl_store:
            return None
        if not self.is_expired(edge_id):
            return None
        return self._ttl_store[edge_id]

    def filter_expired(self, edge_ids):
        return [eid for eid in edge_ids if not self.is_expired(eid)]

    def record_error(self, edge_id, error_type):
        if error_type in ("permission_denied", "forbidden"):
            self.set_ttl(edge_id, "permission_denied")
        elif error_type in ("deprecated", "not_found"):
            self.set_ttl(edge_id, "deprecated_api")
        elif error_type in ("syntax_error", "crash", "exception", "internal_error"):
            self.set_ttl(edge_id, "syntax_crash")

    def snapshot(self):
        """Copy of the whole TTL store (persisted via engine._sync_ttl_store).
        Dict-of-dict so callers can't mutate the live entries."""
        return {eid: dict(entry) for eid, entry in self._ttl_store.items()}

    def restore(self, entries):
        """Load entries previously persisted by the engine (row 2 of
        82inefficiencies.md): a 'don't do this again' mute must survive a
        restart. Expired entries are loaded too — `is_expired` lazily drops
        them on first check, and run_maintenance prunes the DB rows."""
        for edge_id, entry in entries.items():
            if isinstance(entry, dict) and entry.get("expires_at"):
                self._ttl_store[edge_id] = entry
