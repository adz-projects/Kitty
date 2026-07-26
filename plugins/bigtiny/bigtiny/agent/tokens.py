from __future__ import annotations

import json
from typing import Any

try:
    from bigtiny.providers.openai_compat import _get_encoding
except Exception:  # pragma: no cover - tiktoken/model files unavailable
    _get_encoding = None


def _estimate(text: str) -> int:
    return len(text) // 4


def count_text_tokens(text: str) -> int:
    if not text:
        return 0
    if _get_encoding is not None:
        try:
            return len(_get_encoding("gpt-4o").encode(text))
        except Exception:
            pass
    return _estimate(text)


def count_message_tokens(msg: dict[str, Any]) -> int:
    """Token count for one context message, matching what actually gets
    serialized onto the wire for it — unlike the old `_count_tokens`
    (`ContextManager._count_tokens`), which only ever stringified `content`
    and so counted a message's `tool_calls` payload (often the largest part
    of an assistant turn that calls a tool with a big argument blob) as
    zero.
    """
    total = 0

    content = msg.get("content")
    if isinstance(content, str):
        total += count_text_tokens(content)
    elif isinstance(content, list):
        # multimodal content blocks — count text parts, flat per-image cost
        # for the rest (an exact vision tokenizer isn't worth it here).
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "text":
                total += count_text_tokens(block.get("text", ""))
            elif block.get("type") == "image":
                total += 256

    tool_calls = msg.get("tool_calls")
    if tool_calls:
        total += count_text_tokens(json.dumps(tool_calls))

    tool_call_id = msg.get("tool_call_id")
    if tool_call_id:
        total += count_text_tokens(str(tool_call_id))

    # small fixed overhead per message for role/framing tokens
    total += 4
    return total


def count_messages_tokens(messages: list[dict[str, Any]]) -> int:
    return sum(count_message_tokens(m) for m in messages)
