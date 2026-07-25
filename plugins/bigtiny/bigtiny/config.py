from __future__ import annotations

from pathlib import Path
from typing import Literal

from pydantic import BaseModel
from pydantic_settings import BaseSettings
import yaml


class FallbackConfig(BaseModel):
    mode: Literal["priority", "round-robin"] = "priority"
    retry_on_error: bool = True
    max_retries: int = 2
    retry_backoff_ms: int = 1000


class TokenManagementConfig(BaseModel):
    max_context_tokens: int = 128000
    compaction_threshold: float = 0.8
    compaction_target_ratio: float = 0.5


class AgentConfig(BaseModel):
    # Bounds how many tool calls from a single turn run concurrently (e.g. a
    # model calling both a RAG search and a web search at once) — a per-turn
    # cap, not a daemon-wide one; each `Agent.run()` call gets its own fresh
    # semaphore. Guards against a pathological turn with many tool calls
    # firing all of them at once (process spawns, disk/network contention)
    # while still letting the common 2-3-parallel-calls case run unthrottled.
    max_concurrent_tool_calls: int = 5


class HITLConfig(BaseModel):
    default_policy: Literal["always_ask", "auto_allow", "auto_reject"] = "always_ask"
    always_allow_patterns: list[str] = []
    auto_reject_patterns: list[str] = [
        "rm -rf /",
        "chmod 777",
        "dd if=",
        "mkfs",
    ]


class RecipesConfig(BaseModel):
    # Currently unused — `RecipeEngine` is constructed with no explicit
    # `recipes_dir` (`server/app.py`'s `lifespan()`), so its own default
    # (`bigtiny.paths.data_dir()/recipes`) applies instead of this field.
    # Kept in sync with that default for whenever this does get wired in.
    directory: str = "~/.bigtiny/recipes"


class SchedulerConfig(BaseModel):
    enabled: bool = True


class LoggingConfig(BaseModel):
    level: str = "info"
    json_format: bool = True


class ServerConfig(BaseModel):
    host: str = "127.0.0.1"
    port: int = 8080
    reload: bool = False


class BigTinyConfig(BaseSettings):

    fallback: FallbackConfig = FallbackConfig()
    token_management: TokenManagementConfig = TokenManagementConfig()
    agent: AgentConfig = AgentConfig()
    hitl: HITLConfig = HITLConfig()
    recipes: RecipesConfig = RecipesConfig()
    scheduler: SchedulerConfig = SchedulerConfig()
    logging: LoggingConfig = LoggingConfig()
    server: ServerConfig = ServerConfig()


def load_config(path: str | None = None) -> BigTinyConfig:
    if path:
        raw = Path(path).expanduser().read_text()
        data = yaml.safe_load(raw)
        return BigTinyConfig(**data)
    return BigTinyConfig()
