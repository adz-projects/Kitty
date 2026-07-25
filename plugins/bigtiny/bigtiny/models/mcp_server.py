from __future__ import annotations

import enum
from datetime import datetime
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, Field


class TransportType(str, enum.Enum):
    stdio = "stdio"
    sse = "sse"
    # The MCP spec's successor to the old two-endpoint HTTP+SSE transport
    # (separate GET /sse stream + POST /message): a single endpoint that
    # accepts POST requests and responds with either a plain JSON body or one
    # `event: message` SSE frame, with no separate long-lived listener stream.
    streamable_http = "streamable_http"


class MCPServerConfig(BaseModel):
    id: str = Field(default_factory=lambda: uuid4().hex[:8])
    name: str
    transport: TransportType
    command: str | None = None
    args: list[str] | None = None
    # The remote endpoint for `sse` (its GET stream URL) or `streamable_http`
    # (its single POST endpoint) — not used for `stdio`.
    url: str | None = None
    env: dict[str, str] | None = None
    # Extra HTTP headers sent with every request to a `sse`/`streamable_http`
    # server — e.g. `{"Authorization": "Bearer <token>"}` for a server that
    # requires auth (neither remote transport could authenticate at all
    # before this field existed).
    headers: dict[str, str] | None = None
    status: str = "disconnected"
    error_message: str | None = None
    created_at: datetime = Field(default_factory=datetime.utcnow)
    updated_at: datetime = Field(default_factory=datetime.utcnow)


class ToolDefinition(BaseModel):
    name: str
    description: str
    input_schema: dict[str, Any]
    server_id: str


class ToolResult(BaseModel):
    content: str
    tool_call_id: str
    duration_ms: int = 0
    output_size_bytes: int = 0
    is_error: bool = False
    truncated: bool = False
