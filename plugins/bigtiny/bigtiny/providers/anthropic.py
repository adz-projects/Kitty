from __future__ import annotations

import json
import time
from typing import Any, AsyncIterator

import httpx
from tenacity import retry, stop_after_attempt, wait_exponential, retry_if_exception_type

from bigtiny.models.provider import ProviderConfig, ModelInfo, HealthStatus
from bigtiny.models.session import Message
from bigtiny.network import PreferDirectTransport, TailscaleClient
from bigtiny.providers.base import Provider, Delta, ToolCall
from bigtiny.providers.errors import classify_provider_error


class AnthropicProvider(Provider):
    DEFAULT_MODEL = "claude-sonnet-4-20250514"

    def __init__(
        self,
        provider_id: str,
        config: ProviderConfig,
        api_key: str | None = None,
        tailscale: TailscaleClient | None = None,
    ):
        super().__init__(provider_id, config)
        self.api_key = api_key
        transport = PreferDirectTransport(tailscale) if tailscale is not None else None
        self._client = httpx.AsyncClient(
            base_url=config.base_url.rstrip("/") if config.base_url else "https://api.anthropic.com",
            timeout=httpx.Timeout(120.0, connect=5.0, read=120.0),
            headers={
                "x-api-key": api_key or "",
                "anthropic-version": "2023-06-01",
                "content-type": "application/json",
            } if api_key else {"anthropic-version": "2023-06-01"},
            transport=transport,
        )

    async def chat_completion(
        self,
        messages: list[Message],
        tools: list[dict[str, Any]] | None = None,
        temperature: float | None = None,
        top_p: float | None = None,
        max_tokens: int | None = None,
        model: str | None = None,
    ) -> AsyncIterator[Delta]:
        body: dict[str, Any] = {
            "model": self.resolve_model(model),
            "messages": self._build_messages(messages),
            "stream": True,
            # max_tokens is required by the Messages API
            "max_tokens": max_tokens if max_tokens is not None else 4096,
        }
        system_msgs = [m.content for m in messages if m.role.value == "system" and m.content]
        if system_msgs:
            body["system"] = "\n".join(system_msgs)
        if tools:
            body["tools"] = self._convert_tools(tools)
        if temperature is not None:
            body["temperature"] = temperature
        if top_p is not None:
            body["top_p"] = top_p

        # tool_use input streams as input_json_delta fragments; accumulate
        # per block and emit the complete ToolCall at content_block_stop.
        current_tool_use: dict[str, Any] | None = None
        usage: dict[str, int] = {"input_tokens": 0, "output_tokens": 0}

        try:
            async with self._client.stream("POST", "/v1/messages", json=body) as response:
                if response.status_code >= 400:
                    # Read the body while the stream is still open — the
                    # except-branch below runs after __aexit__ has closed it,
                    # where .text raises ResponseNotRead and kills the run.
                    detail = (await response.aread()).decode("utf-8", errors="replace")
                    classified = classify_provider_error(response.status_code, detail)
                    yield Delta(
                        role="assistant",
                        content=classified.user_message,
                        finish_reason="error",
                        error_type=classified.type,
                    )
                    return
                async for line in response.aiter_lines():
                    line = line.strip()
                    if not line or line.startswith(":"):
                        continue
                    if line.startswith("event: "):
                        continue
                    if line.startswith("data: "):
                        data_str = line[6:]
                        try:
                            event = json.loads(data_str)
                        except json.JSONDecodeError:
                            continue

                        event_type = event.get("type")

                        if event_type == "message_start":
                            msg_usage = event.get("message", {}).get("usage", {})
                            usage["input_tokens"] = int(msg_usage.get("input_tokens") or 0)
                            continue

                        if event_type == "message_delta":
                            delta_usage = event.get("usage", {})
                            if delta_usage.get("output_tokens") is not None:
                                usage["output_tokens"] = int(delta_usage["output_tokens"])

                        if event_type == "message_stop":
                            yield Delta(finish_reason="end_turn", usage=dict(usage))
                            continue

                        if event_type == "content_block_start":
                            block = event.get("content_block", {})
                            if block.get("type") == "tool_use":
                                current_tool_use = {
                                    "id": block.get("id", ""),
                                    "name": block.get("name", ""),
                                    "input_json": "",
                                }
                            continue

                        if event_type == "content_block_delta" and current_tool_use is not None:
                            delta_data = event.get("delta", {})
                            if delta_data.get("type") == "input_json_delta":
                                current_tool_use["input_json"] += delta_data.get("partial_json", "")
                            continue

                        if event_type == "content_block_stop" and current_tool_use is not None:
                            yield Delta(
                                tool_calls=[ToolCall(
                                    id=current_tool_use["id"],
                                    type="function",
                                    function={
                                        "name": current_tool_use["name"],
                                        "arguments": current_tool_use["input_json"] or "{}",
                                    },
                                )]
                            )
                            current_tool_use = None
                            continue

                        delta = self._parse_event(event)
                        if delta:
                            yield delta
        except httpx.HTTPStatusError as e:
            # Safety net only (the status check above handles the normal
            # case): never touch e.response.text here — on a streaming
            # response that was never read it raises ResponseNotRead.
            classified = classify_provider_error(e.response.status_code, "")
            yield Delta(
                role="assistant",
                content=classified.user_message,
                finish_reason="error",
                error_type=classified.type,
            )
        except httpx.RequestError as e:
            yield Delta(
                role="assistant",
                content=f"[Anthropic connection error: {e}]",
                finish_reason="error",
            )

    def _build_messages(self, messages: list[Message]) -> list[dict[str, Any]]:
        """Serializes non-system messages for the Messages API, grouping
        consecutive `role == "tool"` messages into a single `user` message
        carrying multiple `tool_result` blocks — Anthropic requires this (it
        rejects back-to-back `user` messages) and it's what actually
        correlates a result back to its `tool_use_id`, instead of the old
        behavior of flattening every tool reply into a standalone plain-text
        `user` message with no id at all. Tool calls always arrive as a
        contiguous run (the agent loop appends them immediately after the
        assistant turn that requested them, before any next user/assistant
        message), so a simple flush-on-non-tool-message pass is sufficient —
        no need to look further ahead."""
        result: list[dict[str, Any]] = []
        pending_tool_results: list[dict[str, Any]] = []

        def flush_tool_results() -> None:
            if pending_tool_results:
                result.append({"role": "user", "content": list(pending_tool_results)})
                pending_tool_results.clear()

        for m in messages:
            if m.role.value == "system":
                continue
            if m.role.value == "tool":
                content = m.content if isinstance(m.content, str) else json.dumps(m.content)
                pending_tool_results.append({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id or "",
                    "content": content,
                })
                continue
            flush_tool_results()
            result.append(self._serialize_message(m))
        flush_tool_results()
        return result

    def _serialize_message(self, msg: Message) -> dict[str, Any]:
        role = msg.role.value
        content: Any = msg.content or ""
        if isinstance(msg.content, list):
            content = self._blocks_to_anthropic(msg.content)

        if role == "assistant" and msg.tool_calls:
            # Represent as content blocks: any text first, then one
            # `tool_use` block per OpenAI-shaped call in `msg.tool_calls` —
            # this is what gives each call an `id` for a later `tool_result`
            # block (built in `_build_messages`) to reference via
            # `tool_use_id`, matching Anthropic's own tool_use/tool_result
            # pairing instead of losing the correlation entirely.
            blocks: list[dict[str, Any]] = []
            if isinstance(content, str) and content:
                blocks.append({"type": "text", "text": content})
            elif isinstance(content, list):
                blocks.extend(content)
            for tc in msg.tool_calls:
                fn = tc.get("function", {})
                raw_args = fn.get("arguments", "{}")
                try:
                    tool_input = json.loads(raw_args) if isinstance(raw_args, str) else raw_args
                except json.JSONDecodeError:
                    tool_input = {}
                blocks.append({
                    "type": "tool_use",
                    "id": tc.get("id", ""),
                    "name": fn.get("name", ""),
                    "input": tool_input,
                })
            return {"role": "assistant", "content": blocks}

        return {"role": role, "content": content}

    @staticmethod
    def _blocks_to_anthropic(blocks: list[dict[str, Any]]) -> list[dict[str, Any]]:
        parts: list[dict[str, Any]] = []
        for b in blocks:
            if b.get("type") == "text":
                parts.append({"type": "text", "text": b.get("text", "")})
            elif b.get("type") == "image":
                parts.append({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": b.get("mime_type", "image/png"),
                        "data": b.get("data", ""),
                    },
                })
        return parts

    def _convert_tools(self, tools: list[dict[str, Any]]) -> list[dict[str, Any]]:
        converted = []
        for tool in tools:
            converted.append({
                "name": tool.get("function", {}).get("name", tool.get("name", "unknown")),
                "description": tool.get("function", {}).get("description", tool.get("description", "")),
                "input_schema": tool.get("function", {}).get("parameters", tool.get("input_schema", {})),
            })
        return converted

    def _parse_event(self, event: dict[str, Any]) -> Delta | None:
        event_type = event.get("type")

        if event_type == "content_block_delta":
            delta = event.get("delta", {})
            if delta.get("type") == "text_delta":
                return Delta(content=delta.get("text", ""))
            if delta.get("type") == "thinking_delta":
                return Delta(reasoning=delta.get("thinking", ""))

        elif event_type == "message_delta":
            delta = event.get("delta", {})
            stop_reason = delta.get("stop_reason") or delta.get("stop_sequence")
            if stop_reason:
                return Delta(finish_reason=stop_reason)

        elif event_type == "ping":
            return None

        return None

    @retry(
        stop=stop_after_attempt(2),
        wait=wait_exponential(multiplier=1, min=1, max=4),
        retry=retry_if_exception_type((httpx.HTTPStatusError, httpx.RequestError)),
    )
    async def discover_models(self) -> list[ModelInfo]:
        response = await self._client.get("/v1/models")
        response.raise_for_status()
        data = response.json()
        models = []
        for item in data.get("data", []):
            models.append(ModelInfo(
                id=item["id"],
                name=item.get("display_name") or item.get("id"),
                provider_id=self.provider_id,
            ))
        return models

    async def count_tokens(self, messages: list[dict]) -> int:
        total = 0
        for msg in messages:
            content = str(msg.get("content", ""))
            total += len(content) // 4
        return total

    async def check_health(self) -> HealthStatus:
        try:
            start = time.time()
            response = await self._client.get("/v1/models")
            latency = (time.time() - start) * 1000
            if response.status_code == 200:
                return HealthStatus(status="healthy", latency_ms=latency)
            return HealthStatus(status="unhealthy", latency_ms=latency, error=f"HTTP {response.status_code}")
        except Exception as e:
            return HealthStatus(status="unhealthy", error=str(e))
