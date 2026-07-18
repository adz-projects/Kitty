import json
import os
import re
import time
import urllib.error
import urllib.request

import mmh3
import numpy as np


_WORD_RE = re.compile(r"[a-z0-9]+")


class EmbeddingProvider:
    """Turns arbitrary context text into a fixed-dimension vector.

    Tries Ollama's embeddings API first (semantic — Kitty already runs
    Ollama locally), falling back to a deterministic signed-hashing
    vectorizer (lexical) when Ollama is unavailable or errors. Without this,
    every `decide`/`record_outcome`/`record_annotation` call embeds a
    constant zero vector, which collapses the context-sensitive bandit into
    a context-free frequency learner (bleeds preferences across unrelated
    topics) and permanently disables domain inference/bleed/novelty.

    Deliberately synchronous (uses stdlib `urllib`, not an async HTTP
    client) — callers run it via `asyncio.to_thread` from the async MCP/
    sidecar handlers, keeping `decide()` itself sync/zero-I/O per the
    package's hard constraints.
    """

    def __init__(self, config, urlopen_fn=None):
        ec = config.get("embedding", {})
        self.dim = config.get("embedding_dim", 384)
        self.ollama_url = os.environ.get("AP_EMBED_OLLAMA_URL") or ec.get(
            "ollama_url", "http://localhost:11434"
        )
        self.ollama_model = os.environ.get("AP_EMBED_OLLAMA_MODEL") or ec.get(
            "ollama_model", "qwen3-embedding:0.6b"
        )
        self.timeout_s = ec.get("timeout_s", 2)
        self.probe_interval_s = ec.get("probe_interval_s", 60)
        self.cache_size = ec.get("cache_size", 256)
        self._urlopen = urlopen_fn or urllib.request.urlopen

        self._ollama_available = None  # None = never probed
        self._last_probe_ts = 0.0
        self._cache = {}
        self._cache_order = []

    def embed(self, text):
        if not text:
            return np.zeros(self.dim, dtype=np.float32)
        text = text.strip()
        if not text:
            return np.zeros(self.dim, dtype=np.float32)

        cached = self._cache.get(text)
        if cached is not None:
            return cached

        vec = self._embed_ollama(text)
        if vec is None:
            vec = self._embed_hashing(text)
        self._cache_put(text, vec)
        return vec

    def probe_ollama(self):
        """Force a fresh availability check (e.g. for a health endpoint)."""
        self._ollama_available = None
        self._last_probe_ts = 0.0
        self._embed_ollama("probe")
        return bool(self._ollama_available)

    def _cache_put(self, text, vec):
        if text in self._cache:
            self._cache_order.remove(text)
        self._cache[text] = vec
        self._cache_order.append(text)
        if len(self._cache_order) > self.cache_size:
            oldest = self._cache_order.pop(0)
            del self._cache[oldest]

    def _should_try_ollama(self):
        if self._ollama_available is True:
            return True
        if self._ollama_available is False:
            return (time.time() - self._last_probe_ts) >= self.probe_interval_s
        return True

    def _embed_ollama(self, text):
        if not self._should_try_ollama():
            return None
        try:
            payload = json.dumps({"model": self.ollama_model, "prompt": text}).encode("utf-8")
            req = urllib.request.Request(
                f"{self.ollama_url}/api/embeddings",
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with self._urlopen(req, timeout=self.timeout_s) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            raw = np.asarray(data.get("embedding", []), dtype=np.float32)
            if raw.size == 0:
                raise ValueError("empty embedding in Ollama response")
            self._ollama_available = True
            self._last_probe_ts = time.time()
            return self._project(raw)
        except (urllib.error.URLError, ValueError, TimeoutError, OSError, json.JSONDecodeError):
            self._ollama_available = False
            self._last_probe_ts = time.time()
            return None

    def _project(self, raw):
        # Resize an arbitrary-length Ollama embedding to self.dim. Models
        # commonly emit 768/1024-dim vectors; folding (wrap-add) rather than
        # truncating keeps information from every source dimension instead
        # of silently dropping the tail.
        if raw.size == self.dim:
            out = raw.astype(np.float32, copy=True)
        elif raw.size > self.dim:
            out = np.zeros(self.dim, dtype=np.float32)
            for i in range(raw.size):
                out[i % self.dim] += raw[i]
        else:
            out = np.zeros(self.dim, dtype=np.float32)
            out[: raw.size] = raw
        norm = float(np.linalg.norm(out))
        return out / norm if norm > 1e-10 else out

    def _embed_hashing(self, text):
        # Deterministic lexical fallback: feature-hashing with a random
        # sign per token (the standard hashing-trick construction) so
        # unrelated vocabularies land far apart while identical text always
        # produces the identical vector.
        tokens = _WORD_RE.findall(text.lower())
        vec = np.zeros(self.dim, dtype=np.float32)
        for tok in tokens:
            idx = mmh3.hash(tok, seed=2026) % self.dim
            sign = 1.0 if mmh3.hash(tok, seed=2027) % 2 == 0 else -1.0
            vec[idx] += sign
        norm = float(np.linalg.norm(vec))
        return vec / norm if norm > 1e-10 else vec
