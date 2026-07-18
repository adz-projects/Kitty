from adaptive_pathway.learning.ttl import EdgeTTL


def _config():
    return {
        "ttl": {
            "auto_enabled": True,
            "permission_denied_hours": 48,
            "deprecated_api_hours": 72,
            "syntax_crash_hours": 24,
            "user_override_clears_ttl": True,
        }
    }


def test_get_ttl_returns_entry_while_active():
    # get_ttl() used to be inverted relative to is_expired(): it returned
    # None while the TTL was actually active, and a stale (already-deleted)
    # entry after it had lapsed.
    ttl = EdgeTTL(_config())
    ttl.set_ttl("edge_a", "permission_denied")
    assert ttl.is_expired("edge_a") is True  # "is_expired" means "actively suppressed"
    entry = ttl.get_ttl("edge_a")
    assert entry is not None
    assert entry["cause"] == "permission_denied"


def test_get_ttl_returns_none_when_no_ttl_set():
    ttl = EdgeTTL(_config())
    assert ttl.get_ttl("never_set") is None


def test_get_ttl_returns_none_after_lapse():
    ttl = EdgeTTL(_config())
    ttl.set_ttl("edge_b", "syntax_crash")
    ttl._ttl_store["edge_b"]["expires_at"] = "2000-01-01T00:00:00"
    assert ttl.is_expired("edge_b") is False  # lapsed -> deleted
    assert ttl.get_ttl("edge_b") is None


def test_clear_ttl_removes_entry():
    ttl = EdgeTTL(_config())
    ttl.set_ttl("edge_c", "deprecated_api")
    ttl.clear_ttl("edge_c")
    assert ttl.get_ttl("edge_c") is None
    assert ttl.is_expired("edge_c") is False


def test_user_rejected_cause_uses_configured_long_duration():
    # The topic-level "stop suggesting this" lever: a moderate+ dont_do_again
    # sets a long TTL (default 30 days) via this cause, distinct from the
    # short tool-error causes above.
    from datetime import datetime
    config = _config()
    config["ttl"]["user_dont_do_again_hours"] = 720
    ttl = EdgeTTL(config)
    ttl.set_ttl("edge_d", "user_rejected")
    entry = ttl.get_ttl("edge_d")
    assert entry is not None
    assert entry["cause"] == "user_rejected"
    expires = datetime.fromisoformat(entry["expires_at"])
    set_at = datetime.fromisoformat(entry["set_at"])
    assert (expires - set_at).total_seconds() == 720 * 3600


def test_user_rejected_defaults_to_720_hours_when_unconfigured():
    ttl = EdgeTTL(_config())  # no user_dont_do_again_hours key in config
    assert ttl.user_dont_do_again_hours == 720
