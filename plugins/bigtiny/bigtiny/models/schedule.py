from __future__ import annotations

from datetime import datetime
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, Field


class JobConfig(BaseModel):
    name: str
    cron: str
    recipe_id: str
    parameters: dict[str, Any] | None = None
    enabled: bool = True


class ScheduleJob(BaseModel):
    id: str = Field(default_factory=lambda: uuid4().hex[:8])
    name: str
    cron: str
    recipe_id: str
    parameters: dict[str, Any] | None = None
    enabled: bool = True
    created_at: datetime = Field(default_factory=datetime.utcnow)
    updated_at: datetime = Field(default_factory=datetime.utcnow)
