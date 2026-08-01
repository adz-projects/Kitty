from __future__ import annotations

import json
from typing import Any, Literal

from pydantic import BaseModel

ProviderErrorType = Literal["context_exceeded", "insufficient_credits", "other"]

_CONTEXT_MESSAGE_MARKERS = (
    "context length",
    "maximum context",
    "context_length",
    "too large",
    "too long",
    "maximum",
    "prompt is too long",
    "exceeds",
)

_CREDITS_MESSAGE_MARKERS = ("billing", "overage", "quota", "credit")

_CONTEXT_USER_MESSAGE = (
    "The conversation has exceeded the model's context limit. Try starting a new "
    "session or enabling compaction to summarize older messages."
)
_CREDITS_USER_MESSAGE = (
    "Your API credits are exhausted. Check your provider's billing settings or "
    "switch to another provider."
)


class ProviderError(BaseModel):
    type: ProviderErrorType
    user_message: str
    raw_message: str
    http_status: int


def _extract_message(body: str) -> tuple[str, dict[str, Any] | None]:
    """Returns (message_text, parsed_error_dict). Falls back to the raw body
    as the message when it isn't JSON or doesn't have the expected shape —
    classification below still works off the raw text in that case."""
    try:
        parsed = json.loads(body)
    except (json.JSONDecodeError, TypeError):
        return body, None
    if isinstance(parsed, dict):
        error = parsed.get("error")
        if isinstance(error, dict):
            message = error.get("message")
            return (str(message) if message else body), error
        if isinstance(error, str):
            return error, None
    return body, None


def classify_provider_error(status_code: int, body: str) -> ProviderError:
    """Classifies a provider HTTP error into a small set of user-actionable
    types. Falls through to "other" (original message, not fabricated
    guidance) for anything unrecognised — better an honest raw message than a
    wrong diagnosis."""
    raw_message, error_obj = _extract_message(body)
    lower_message = raw_message.lower()
    error_type = (error_obj or {}).get("type") if error_obj else None
    error_code = (error_obj or {}).get("code") if error_obj else None

    # Insufficient credits: OpenAI/Anthropic `error.type == "insufficient_quota"`,
    # HTTP 402, or billing/quota/credit language in the message.
    if (
        error_type == "insufficient_quota"
        or status_code == 402
        or any(marker in lower_message for marker in _CREDITS_MESSAGE_MARKERS)
    ):
        return ProviderError(
            type="insufficient_credits",
            user_message=_CREDITS_USER_MESSAGE,
            raw_message=raw_message,
            http_status=status_code,
        )

    # Context exceeded: OpenAI `error.code == "context_length_exceeded"`, or
    # context/maximum/too-long language in the message (covers Anthropic and
    # generic OpenAI-compat backends like Ollama/vLLM, which don't send a
    # structured code).
    if error_code == "context_length_exceeded" or any(
        marker in lower_message for marker in _CONTEXT_MESSAGE_MARKERS
    ):
        return ProviderError(
            type="context_exceeded",
            user_message=_CONTEXT_USER_MESSAGE,
            raw_message=raw_message,
            http_status=status_code,
        )

    return ProviderError(
        type="other",
        user_message=raw_message or f"Provider error (HTTP {status_code})",
        raw_message=raw_message,
        http_status=status_code,
    )
