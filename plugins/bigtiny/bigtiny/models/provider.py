from __future__ import annotations

import enum
from datetime import datetime
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, Field


class ProviderType(str, enum.Enum):
    openai_compat = "openai_compat"
    anthropic = "anthropic"


class ProviderConfig(BaseModel):
    id: str = Field(default_factory=lambda: uuid4().hex[:8])
    name: str
    provider_type: ProviderType
    base_url: str
    fallback_priority: int = 1
    config: dict[str, Any] | None = None
    status: str = "disconnected"
    error_message: str | None = None
    created_at: datetime = Field(default_factory=datetime.utcnow)
    updated_at: datetime = Field(default_factory=datetime.utcnow)


class ModelInfo(BaseModel):
    id: str
    name: str | None = None
    provider_id: str | None = None
    context_length: int | None = None


class HealthStatus(BaseModel):
    status: str  # "healthy", "unhealthy", "unknown"
    latency_ms: float | None = None
    error: str | None = None
