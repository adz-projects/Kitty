from __future__ import annotations

import enum
from datetime import datetime
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, Field


class MessageRole(str, enum.Enum):
    user = "user"
    assistant = "assistant"
    system = "system"
    tool = "tool"


class Message(BaseModel):
    id: str = Field(default_factory=lambda: uuid4().hex)
    session_id: str
    role: MessageRole
    # str for plain text; list of content blocks for multimodal, e.g.
    # [{"type": "text", "text": ...}, {"type": "image", "data": <b64>, "mime_type": ...}]
    content: str | list[dict[str, Any]] | None = None
    tool_calls: list[dict[str, Any]] | None = None
    tool_call_id: str | None = None  # set on role="tool" messages
    token_count: int = 0
    created_at: datetime = Field(default_factory=datetime.utcnow)

    def model_dump(self, **kwargs) -> dict[str, Any]:
        kwargs.setdefault("mode", "json")
        return super().model_dump(**kwargs)


class Session(BaseModel):
    id: str = Field(default_factory=lambda: uuid4().hex)
    name: str | None = None
    created_at: datetime = Field(default_factory=datetime.utcnow)
    updated_at: datetime = Field(default_factory=datetime.utcnow)
    status: str = "active"
    metadata: dict[str, Any] | None = None
