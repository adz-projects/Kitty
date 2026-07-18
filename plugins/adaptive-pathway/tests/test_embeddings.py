import json

import numpy as np
import pytest
import yaml
from pathlib import Path

from adaptive_pathway.embeddings import EmbeddingProvider


def _load_config():
    config_path = Path(__file__).parent.parent / "src" / "adaptive_pathway" / "config" / "defaults.yaml"
    with open(config_path) as f:
        return yaml.safe_load(f)


def _always_fail_urlopen(req, timeout=None):
    raise OSError("simulated Ollama unavailable")


class _FakeResponse:
    def __init__(self, payload):
        self._payload = payload

    def read(self):
        return json.dumps(self._payload).encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


def _fake_ollama_urlopen(embedding_dim=768, seed=42):
    rng = np.random.default_rng(seed)
    vec = rng.standard_normal(embedding_dim).tolist()

    def _urlopen(req, timeout=None):
        return _FakeResponse({"embedding": vec})

    return _urlopen


# ─── Hashing fallback (Ollama unavailable) ─────────────────────────────────


def test_hashing_fallback_is_deterministic():
    ep = EmbeddingProvider(_load_config(), urlopen_fn=_always_fail_urlopen)
    v1 = ep.embed("reviewing my novel draft about violence")
    v2 = ep.embed("reviewing my novel draft about violence")
    assert np.array_equal(v1, v2)


def test_hashing_fallback_distinguishes_unrelated_topics():
    # This is the core fix for the frequency-bleed scenario: without real
    # embeddings, every context looked identical to the bandit.
    ep = EmbeddingProvider(_load_config(), urlopen_fn=_always_fail_urlopen)
    v1 = ep.embed("reviewing my novel draft about violence")
    v2 = ep.embed("writing a privacy policy for a service")
    assert not np.allclose(v1, v2)
    assert float(np.dot(v1, v2)) < 0.5


def test_hashing_fallback_correct_dim_and_unit_norm():
    config = _load_config()
    ep = EmbeddingProvider(config, urlopen_fn=_always_fail_urlopen)
    v = ep.embed("some context text")
    assert v.shape == (config["embedding_dim"],)
    assert v.dtype == np.float32
    assert abs(float(np.linalg.norm(v)) - 1.0) < 1e-4


def test_empty_or_whitespace_text_returns_zeros():
    ep = EmbeddingProvider(_load_config(), urlopen_fn=_always_fail_urlopen)
    assert np.allclose(ep.embed(""), 0)
    assert np.allclose(ep.embed("   "), 0)


def test_ollama_marked_unavailable_after_failure():
    ep = EmbeddingProvider(_load_config(), urlopen_fn=_always_fail_urlopen)
    ep.embed("anything")
    assert ep._ollama_available is False


# ─── Ollama backend (mocked) ────────────────────────────────────────────────


def test_ollama_path_used_when_available():
    config = _load_config()
    ep = EmbeddingProvider(config, urlopen_fn=_fake_ollama_urlopen(embedding_dim=768))
    v = ep.embed("test context")
    assert v.shape == (config["embedding_dim"],)
    assert ep._ollama_available is True
    assert abs(float(np.linalg.norm(v)) - 1.0) < 1e-4


def test_ollama_exact_dim_response_passes_through():
    config = _load_config()
    ep = EmbeddingProvider(config, urlopen_fn=_fake_ollama_urlopen(embedding_dim=config["embedding_dim"]))
    v = ep.embed("test context")
    assert v.shape == (config["embedding_dim"],)


def test_ollama_smaller_dim_response_gets_padded():
    config = _load_config()
    ep = EmbeddingProvider(config, urlopen_fn=_fake_ollama_urlopen(embedding_dim=100))
    v = ep.embed("test context")
    assert v.shape == (config["embedding_dim"],)


def test_probe_ollama_forces_fresh_check():
    ep = EmbeddingProvider(_load_config(), urlopen_fn=_always_fail_urlopen)
    assert ep.probe_ollama() is False


# ─── Cache ───────────────────────────────────────────────────────────────


def test_repeated_text_hits_cache_not_transport():
    calls = {"n": 0}

    def counting_urlopen(req, timeout=None):
        calls["n"] += 1
        raise OSError("no ollama")

    ep = EmbeddingProvider(_load_config(), urlopen_fn=counting_urlopen)
    ep.embed("same context")
    ep.embed("same context")
    ep.embed("same context")
    # First call probes Ollama once (and fails -> hashing fallback, cached).
    # Subsequent identical-text calls must hit the cache, not retry the
    # transport at all (separate from the probe-interval throttle below).
    assert calls["n"] == 1


def test_cache_eviction_respects_max_size():
    config = _load_config()
    config["embedding"]["cache_size"] = 3
    ep = EmbeddingProvider(config, urlopen_fn=_always_fail_urlopen)
    for i in range(5):
        ep.embed(f"context {i}")
    assert len(ep._cache) == 3


# ─── Availability probe throttling ─────────────────────────────────────────


def test_probe_interval_throttles_retries_after_failure():
    calls = {"n": 0}

    def counting_urlopen(req, timeout=None):
        calls["n"] += 1
        raise OSError("no ollama")

    config = _load_config()
    config["embedding"]["probe_interval_s"] = 9999  # effectively "don't retry"
    ep = EmbeddingProvider(config, urlopen_fn=counting_urlopen)
    ep.embed("context a")
    ep.embed("context b")  # different text -> cache miss, but ollama shouldn't be retried
    assert calls["n"] == 1


# ─── Cross-compatible default model + Kitty env-var overrides ─────────────


def test_default_model_is_qwen3_embedding_0_6b():
    ep = EmbeddingProvider(_load_config(), urlopen_fn=_always_fail_urlopen)
    assert ep.ollama_model == "qwen3-embedding:0.6b"


def test_env_var_overrides_win_over_config(monkeypatch):
    monkeypatch.setenv("AP_EMBED_OLLAMA_URL", "http://envhost:9999")
    monkeypatch.setenv("AP_EMBED_OLLAMA_MODEL", "some-other-model")
    config = _load_config()
    assert config["embedding"]["ollama_url"] != "http://envhost:9999"
    ep = EmbeddingProvider(config, urlopen_fn=_always_fail_urlopen)
    assert ep.ollama_url == "http://envhost:9999"
    assert ep.ollama_model == "some-other-model"


def test_no_env_vars_falls_back_to_config(monkeypatch):
    monkeypatch.delenv("AP_EMBED_OLLAMA_URL", raising=False)
    monkeypatch.delenv("AP_EMBED_OLLAMA_MODEL", raising=False)
    config = _load_config()
    ep = EmbeddingProvider(config, urlopen_fn=_always_fail_urlopen)
    assert ep.ollama_url == config["embedding"]["ollama_url"]
    assert ep.ollama_model == config["embedding"]["ollama_model"]
