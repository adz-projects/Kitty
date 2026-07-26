from __future__ import annotations

from pathlib import Path
from typing import Any, Literal

from pydantic import BaseModel
from pydantic_settings import BaseSettings, SettingsConfigDict
import yaml


class FallbackConfig(BaseModel):
    mode: Literal["priority", "round-robin"] = "priority"
    retry_on_error: bool = True
    max_retries: int = 2
    retry_backoff_ms: int = 1000


class TokenManagementConfig(BaseModel):
    max_context_tokens: int = 128000
    # High-water mark: a compaction pass is triggered once the assembled
    # prompt exceeds max_context_tokens * compaction_threshold.
    compaction_threshold: float = 0.8
    # Low-water mark: a triggered pass compacts enough of the candidate span
    # to bring the session back under max_context_tokens * compaction_target_ratio.
    # Having a lower target than the trigger (hysteresis) means compaction
    # runs rarely and deeply rather than on every turn near the threshold —
    # each pass moves the watermark, which invalidates the model's KV
    # prefix cache, so fewer, bigger passes are strictly better than many
    # small ones.
    compaction_target_ratio: float = 0.5
    # Tier-1 deterministic tool-output masking: content longer than
    # head + tail bytes is elided down to its head and tail, keeping both
    # ends since the informative part of tool output (e.g. a traceback's
    # final exception line) often clusters at the tail, not just the head.
    tool_mask_head: int = 400
    tool_mask_tail: int = 400


class SummarizerConfig(BaseModel):
    enabled: bool = True
    model: str = "qwen3.5:0.8b"
    # Ollama's native /api/chat base URL — deliberately not the OpenAI-
    # compat /v1 shim used by the main provider layer, since only the
    # native endpoint accepts `think`, `keep_alive`, and a JSON schema
    # `format`.
    base_url: str = "http://127.0.0.1:11434"
    # Ollama keep_alive value: "0" unloads immediately after the call,
    # "5m" keeps it resident for 5 minutes of idle time, "-1" pins it in
    # VRAM until the daemon exits.
    keep_alive: str = "5m"
    temperature: float = 0.1
    timeout_s: float = 30.0
    # Reserve the last N complete user/assistant exchanges from
    # compaction — they stay in the live, uncompacted window.
    reserve_exchanges: int = 3
    # A memory-slot list longer than this triggers a bounded
    # single-list consolidation pass rather than growing unbounded.
    max_slot_items: int = 20


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
    # env_nested_delimiter lets a nested field be set as e.g.
    # BIGTINY_TOKEN_MANAGEMENT__MAX_CONTEXT_TOKENS=4000. This is how Kitty
    # (which spawns this daemon as a child process and only ever passes
    # environment variables, never --config) configures compaction knobs.
    model_config = SettingsConfigDict(
        env_prefix="BIGTINY_", env_nested_delimiter="__"
    )

    fallback: FallbackConfig = FallbackConfig()
    token_management: TokenManagementConfig = TokenManagementConfig()
    summarizer: SummarizerConfig = SummarizerConfig()
    agent: AgentConfig = AgentConfig()
    hitl: HITLConfig = HITLConfig()
    recipes: RecipesConfig = RecipesConfig()
    scheduler: SchedulerConfig = SchedulerConfig()
    logging: LoggingConfig = LoggingConfig()
    server: ServerConfig = ServerConfig()


def _deep_merge(base: dict[str, Any], override: dict[str, Any]) -> dict[str, Any]:
    merged = dict(base)
    for key, value in override.items():
        if isinstance(value, dict) and isinstance(merged.get(key), dict):
            merged[key] = _deep_merge(merged[key], value)
        else:
            merged[key] = value
    return merged


def load_config(path: str | None = None) -> BigTinyConfig:
    # Env vars (BIGTINY_*) always apply — BaseSettings reads them on
    # construction regardless of --config. When a YAML file is also given,
    # its values are merged on top field-by-field (not splatted wholesale
    # via BigTinyConfig(**data)), so a YAML file that only sets
    # `summarizer.model` doesn't clobber env-derived `summarizer.keep_alive`
    # or any other sibling field with that section's defaults.
    base = BigTinyConfig()
    if not path:
        return base
    raw = Path(path).expanduser().read_text()
    data = yaml.safe_load(raw) or {}
    merged = _deep_merge(base.model_dump(), data)
    return BigTinyConfig(**merged)
