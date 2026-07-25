from __future__ import annotations

from datetime import datetime
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, Field


class RecipeParameter(BaseModel):
    name: str
    type: str = "string"
    description: str | None = None
    required: bool = False
    default: Any = None


class Recipe(BaseModel):
    id: str = Field(default_factory=lambda: uuid4().hex[:8])
    name: str
    prompt_template: str
    instructions: str | None = None
    parameters: list[RecipeParameter] | None = None
    required_mcp_servers: list[str] | None = None
    system_prompt_layer: str | None = None
    max_steps: int = 30
    created_at: datetime = Field(default_factory=datetime.utcnow)
    updated_at: datetime = Field(default_factory=datetime.utcnow)
