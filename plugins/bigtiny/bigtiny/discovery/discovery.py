from __future__ import annotations

import logging
from datetime import datetime
from typing import Any

from bigtiny.models.provider import ModelInfo
from bigtiny.providers.router import ProviderRouter
from bigtiny.storage import Database

logger = logging.getLogger(__name__)


class LocalModelDiscovery:
    def __init__(self, db: Database, router: ProviderRouter):
        self.db = db
        self.router = router
        self._cache: dict[str, tuple[datetime, list[ModelInfo]]] = {}
        self._cache_ttl = 60

    async def discover_all(self) -> list[ModelInfo]:
        all_models: list[ModelInfo] = []
        seen_ids: set[str] = set()
        for provider_id in self.router.get_provider_ids():
            models = await self._discover_provider(provider_id)
            for m in models:
                if m.id not in seen_ids:
                    seen_ids.add(m.id)
                    all_models.append(m)
        return all_models

    async def _discover_provider(self, provider_id: str) -> list[ModelInfo]:
        now = datetime.utcnow()
        if provider_id in self._cache:
            cached_at, models = self._cache[provider_id]
            if (now - cached_at).total_seconds() < self._cache_ttl:
                return models

        provider = self.router._providers.get(provider_id)
        if not provider:
            return []

        try:
            models = await provider.discover_models()
            self._cache[provider_id] = (now, models)
            return models
        except Exception as e:
            logger.warning("Discovery failed for provider %s: %s", provider_id, e)
            return []

    async def discover_provider(self, provider_id: str) -> list[ModelInfo]:
        self._cache.pop(provider_id, None)
        return await self._discover_provider(provider_id)

    def invalidate_cache(self, provider_id: str | None = None) -> None:
        if provider_id:
            self._cache.pop(provider_id, None)
        else:
            self._cache.clear()
