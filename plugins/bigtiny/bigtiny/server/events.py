from __future__ import annotations

import json
from dataclasses import dataclass, asdict
from typing import Any, Literal


SSEEventType = Literal[
    "llm_delta",
    "reasoning_delta",
    "llm_stop",
    "tool_start",
    "tool_finish",
    "hitl_pause",
    "hitl_resolved",
    "error",
    "model_failover",
    "subagent_status",
    "session_status",
    "session_title",
    "compaction",
    "provider_error",
    "llm_timing",
]


@dataclass
class SSEEvent:
    type: SSEEventType
    content: str | None = None
    tool_name: str | None = None
    tool_args: dict[str, Any] | None = None
    tool_result: str | None = None
    duration_ms: int | None = None
    session_id: str | None = None
    usage: dict[str, int] | None = None  # on llm_stop: {"input_tokens", "output_tokens"}
    action_id: str | None = None  # on hitl_pause: id to POST back to /approve
    is_last: bool = False
    error_code: str | None = None
    error_message: str | None = None
    recoverable: bool = True
    error_type: str | None = None  # on provider_error: see classify_provider_error
    # on llm_timing: see agent.loop.TimingResult / stream_with_timing
    ttfb_ms: float | None = None
    ttft_ms: float | None = None
    generation_ms: float | None = None
    provider_id: str | None = None
    model: str | None = None
    total_tokens: int | None = None


def serialize_sse(event: SSEEvent) -> str:
    """Serialize SSEEvent to SSE wire format."""
    payload = json.dumps(asdict(event), default=str)
    return f"data: {payload}\n\n"
