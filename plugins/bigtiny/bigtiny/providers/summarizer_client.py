from __future__ import annotations

import json
import logging
from typing import Any

import httpx

from bigtiny.config import SummarizerConfig

logger = logging.getLogger(__name__)


class SummarizerError(Exception):
    """Raised for any failure calling the summarizer model — connection
    refused, timeout, non-2xx, or a response that fails schema validation.
    Callers must treat this as "skip this compaction pass", never as a
    reason to fail the user's turn."""


class SummarizerClient:
    """Talks to Ollama's *native* `/api/chat` endpoint, not the OpenAI-
    compatible `/v1/chat/completions` shim the main provider layer uses
    (`OpenAICompatibleProvider`). Only the native endpoint accepts `think`
    (disables reasoning-model thinking traces), `keep_alive` (VRAM
    retention), and a JSON-schema `format` (real structured output,
    stronger than the OpenAI-compat layer's `format: "json"`/
    `response_format`, which a sub-1B model needs to reliably emit
    well-formed output).

    A single instance is safe to reuse across calls/sessions — it just
    wraps one httpx client bound to the configured base_url.
    """

    def __init__(self, config: SummarizerConfig):
        self.config = config
        self._client = httpx.AsyncClient(
            base_url=config.base_url.rstrip("/"),
            timeout=httpx.Timeout(config.timeout_s, connect=3.0),
        )

    async def aclose(self) -> None:
        await self._client.aclose()

    async def structured_chat(
        self,
        messages: list[dict[str, Any]],
        json_schema: dict[str, Any],
    ) -> dict[str, Any]:
        """Runs one non-streaming completion constrained to `json_schema`
        and returns the parsed object. Raises SummarizerError on any
        failure — connection error, timeout, non-2xx, or invalid JSON —
        so the caller can leave existing state untouched and retry on a
        later turn rather than corrupt the session's memory.
        """
        body = {
            "model": self.config.model,
            "messages": messages,
            "stream": False,
            "think": False,
            "format": json_schema,
            "keep_alive": self.config.keep_alive,
            "options": {"temperature": self.config.temperature},
        }
        try:
            response = await self._client.post("/api/chat", json=body)
        except httpx.RequestError as e:
            raise SummarizerError(f"connection error: {e}") from e

        if response.status_code >= 400:
            raise SummarizerError(
                f"summarizer HTTP {response.status_code}: {response.text[:200]}"
            )

        try:
            payload = response.json()
        except json.JSONDecodeError as e:
            raise SummarizerError(f"non-JSON response envelope: {e}") from e

        content = (payload.get("message") or {}).get("content")
        if not content:
            raise SummarizerError("empty summarizer response")

        try:
            parsed = json.loads(content)
        except json.JSONDecodeError as e:
            raise SummarizerError(f"summarizer did not return valid JSON: {e}") from e

        if not isinstance(parsed, dict):
            raise SummarizerError("summarizer response was not a JSON object")

        return parsed
