from __future__ import annotations

from abc import ABC, abstractmethod
from typing import Any, AsyncIterator

from pydantic import BaseModel, Field

from bigtiny.models.provider import ProviderConfig, ModelInfo, HealthStatus
from bigtiny.models.session import Message


class DeltaRole:
    assistant = "assistant"
    tool = "tool"


class ToolCall(BaseModel):
    id: str
    type: str = "function"
    function: dict[str, Any]


class Delta(BaseModel):
    role: str = DeltaRole.assistant
    content: str | None = None
    reasoning: str | None = None  # thinking/reasoning channel text
    tool_calls: list[ToolCall] | None = None
    finish_reason: str | None = None
    usage: dict[str, int] | None = None  # {"input_tokens": N, "output_tokens": N}
    # Set only alongside finish_reason="error" — see classify_provider_error.
    error_type: str | None = None


class Provider(ABC):
    #: fallback model id used when neither config nor caller specifies one
    DEFAULT_MODEL = ""

    def __init__(self, provider_id: str, config: ProviderConfig):
        self.provider_id = provider_id
        self.config = config

    def resolve_model(self, override: str | None = None) -> str:
        if override:
            return override
        configured = (self.config.config or {}).get("model")
        return configured or self.DEFAULT_MODEL

    @abstractmethod
    async def chat_completion(
        self,
        messages: list[Message],
        tools: list[dict[str, Any]] | None = None,
        temperature: float | None = None,
        top_p: float | None = None,
        max_tokens: int | None = None,
        model: str | None = None,
    ) -> AsyncIterator[Delta]:
        ...

    @abstractmethod
    async def discover_models(self) -> list[ModelInfo]:
        ...

    @abstractmethod
    async def count_tokens(self, messages: list[dict]) -> int:
        ...

    @abstractmethod
    async def check_health(self) -> HealthStatus:
        ...
