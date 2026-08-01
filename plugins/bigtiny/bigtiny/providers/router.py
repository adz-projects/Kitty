from __future__ import annotations

import asyncio
import json
import logging
import time
from typing import Any

import keyring

from bigtiny.models.provider import ProviderConfig, ProviderType, HealthStatus
from bigtiny.network import TailscaleClient
from bigtiny.providers.base import Provider
from bigtiny.providers.anthropic import AnthropicProvider
from bigtiny.providers.openai_compat import OpenAICompatibleProvider
from bigtiny.storage import Database

logger = logging.getLogger(__name__)


class NoHealthyProvider(Exception):
    pass


class ProviderRouter:
    def __init__(self, db: Database, tailscale: TailscaleClient | None = None):
        self.db = db
        # Shared across every provider this router constructs, so peer
        # discovery and resolved-address caching (TailscaleClient's own
        # doc comment) happen once per daemon run, not once per provider.
        self.tailscale = tailscale or TailscaleClient()
        self._providers: dict[str, Provider] = {}
        self._health_cache: dict[str, HealthStatus] = {}
        self._health_cache_time: dict[str, float] = {}
        # get_provider runs before every LLM turn; without a TTL each turn
        # costs a network round-trip per provider just to re-check health.
        self._health_ttl_sec = 30.0

    async def load_providers(self) -> None:
        rows = await self.db.fetch_all(
            "SELECT * FROM providers ORDER BY fallback_priority ASC"
        )
        for row in rows:
            api_key = self._get_api_key(row["id"])
            provider = self._instantiate(row, api_key)
            self._providers[row["id"]] = provider

    def _get_api_key(self, provider_id: str) -> str | None:
        try:
            return keyring.get_password("bigtiny", f"{provider_id}_api_key")
        except Exception:
            return None

    def _instantiate(self, row: dict[str, Any], api_key: str | None) -> Provider:
        # The config column carries per-provider settings (notably "model");
        # DB rows store it as a JSON string, API callers may pass a dict.
        raw_config = row.get("config")
        if isinstance(raw_config, str):
            try:
                raw_config = json.loads(raw_config)
            except ValueError:
                raw_config = None
        config = ProviderConfig(
            id=row["id"],
            name=row["name"],
            provider_type=ProviderType(row["provider_type"]),
            base_url=row["base_url"],
            fallback_priority=row.get("fallback_priority", 1),
            status=row.get("status", "disconnected"),
            config=raw_config if isinstance(raw_config, dict) else None,
        )

        if config.provider_type == ProviderType.anthropic:
            return AnthropicProvider(row["id"], config, api_key, tailscale=self.tailscale)
        return OpenAICompatibleProvider(row["id"], config, api_key, tailscale=self.tailscale)

    async def _get_health(self, provider: Provider) -> HealthStatus:
        now = time.monotonic()
        cached = self._health_cache.get(provider.provider_id)
        cached_at = self._health_cache_time.get(provider.provider_id, 0.0)
        if cached is not None and now - cached_at < self._health_ttl_sec:
            return cached
        health = await provider.check_health()
        self._health_cache[provider.provider_id] = health
        self._health_cache_time[provider.provider_id] = now
        return health

    async def get_provider(self, preferred_id: str | None = None) -> Provider:
        if preferred_id and preferred_id in self._providers:
            provider = self._providers[preferred_id]
            health = await self._get_health(provider)
            if health.status == "healthy":
                return provider

        sorted_providers = sorted(
            self._providers.values(),
            key=lambda p: p.config.fallback_priority,
        )

        for provider in sorted_providers:
            if preferred_id and provider.provider_id == preferred_id:
                continue
            health = await self._get_health(provider)
            if health.status == "healthy":
                return provider

        raise NoHealthyProvider("No healthy providers available")

    async def check_all_health(self) -> dict[str, HealthStatus]:
        results: dict[str, HealthStatus] = {}

        async def _check(p: Provider) -> tuple[str, HealthStatus]:
            status = await p.check_health()
            return p.provider_id, status

        tasks = [_check(p) for p in self._providers.values()]
        for future in asyncio.as_completed(tasks):
            pid, status = await future
            results[pid] = status
            self._health_cache[pid] = status
            self._health_cache_time[pid] = time.monotonic()

        return results

    def get_provider_ids(self) -> list[str]:
        return list(self._providers.keys())

    async def register_provider(self, row: dict[str, Any], api_key: str | None = None) -> None:
        if api_key is None:
            api_key = self._get_api_key(row["id"])
        provider = self._instantiate(row, api_key)
        self._providers[row["id"]] = provider
        self._health_cache.pop(row["id"], None)
        self._health_cache_time.pop(row["id"], None)

    def unregister_provider(self, provider_id: str) -> None:
        self._providers.pop(provider_id, None)
        self._health_cache.pop(provider_id, None)
        self._health_cache_time.pop(provider_id, None)
