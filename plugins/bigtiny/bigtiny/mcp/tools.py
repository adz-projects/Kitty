from __future__ import annotations

import json
from typing import Any

from jsonschema import validate, ValidationError

from bigtiny.models.mcp_server import ToolDefinition


def validate_tool_args(tool_def: ToolDefinition, args: dict[str, Any]) -> dict[str, Any]:
    if not tool_def.input_schema:
        return args

    try:
        validate(instance=args, schema=tool_def.input_schema)
    except ValidationError as e:
        path = " -> ".join(str(p) for p in e.path) if e.path else "root"
        raise ValueError(
            f"Validation failed for tool '{tool_def.name}' at {path}: {e.message}"
        )

    return args


MAX_TOOL_OUTPUT_BYTES = 100 * 1024  # 100KB

TRUNCATION_MESSAGE = (
    "[Output truncated at 100KB. "
    "Use server-specific pagination to retrieve full data.]"
)


def truncate_output(content: str, max_bytes: int = MAX_TOOL_OUTPUT_BYTES) -> tuple[str, bool]:
    encoded = content.encode("utf-8")
    if len(encoded) <= max_bytes:
        return content, False
    truncated = encoded[:max_bytes].decode("utf-8", errors="ignore")
    truncated += f"\n{TRUNCATION_MESSAGE}"
    return truncated, True
