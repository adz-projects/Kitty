from __future__ import annotations

import json
import time
from typing import Any, AsyncIterator

import httpx
import tiktoken
from tenacity import retry, stop_after_attempt, wait_exponential, retry_if_exception_type

from bigtiny.models.provider import ProviderConfig, ModelInfo, HealthStatus
from bigtiny.models.session import Message
from bigtiny.network import PreferDirectTransport, TailscaleClient
from bigtiny.providers.base import Provider, Delta, ToolCall
from bigtiny.providers.errors import classify_provider_error


ENCODING_CACHE: dict[str, tiktoken.Encoding] = {}


def _get_encoding(model: str) -> tiktoken.Encoding:
    if model not in ENCODING_CACHE:
        try:
            ENCODING_CACHE[model] = tiktoken.encoding_for_model(model)
        except KeyError:
            ENCODING_CACHE[model] = tiktoken.get_encoding("cl100k_base")
    return ENCODING_CACHE[model]


_THINK_OPEN = "<think>"
_THINK_CLOSE = "</think>"
_MAX_TAG_LEN = max(len(_THINK_OPEN), len(_THINK_CLOSE))


def _partial_tag_holdback(buf: str, tag: str) -> int:
    """Length of the longest suffix of `buf` that is a strict prefix of
    `tag` — used to avoid emitting a tag that's been split across two stream
    chunks (e.g. one chunk ends "...<th" and the next starts "ink>...")."""
    for k in range(min(len(tag) - 1, len(buf)), 0, -1):
        if buf.endswith(tag[:k]):
            return k
    return 0


class ThinkTagSplitter:
    """Splits literal inline `<think>...</think>` reasoning out of a raw
    content stream into a separate reasoning channel. Some OpenAI-compatible
    backends — observed via Ollama's `/v1/chat/completions` endpoint for
    reasoning models such as qwen3/deepseek-r1/gpt-oss — emit reasoning as
    plain tags embedded in `content` rather than a distinct
    `reasoning`/`reasoning_content` delta field (unlike DeepSeek's/vLLM's own
    API, which `_parse_chunk` already reads directly), so nothing upstream
    routes it to the reasoning channel without this. Chunk-boundary safe: a
    tag split across two stream deltas is buffered via `_partial_tag_holdback`
    and resolved once the rest arrives, rather than leaking the partial
    marker into either channel."""

    def __init__(self) -> None:
        self._buf = ""
        self._in_think = False

    def feed(self, content: str) -> tuple[str, str]:
        self._buf += content
        out_content: list[str] = []
        out_reasoning: list[str] = []
        while True:
            tag = _THINK_CLOSE if self._in_think else _THINK_OPEN
            idx = self._buf.find(tag)
            if idx == -1:
                holdback = _partial_tag_holdback(self._buf, tag)
                emit_to = len(self._buf) - holdback
                emit, self._buf = self._buf[:emit_to], self._buf[emit_to:]
                (out_reasoning if self._in_think else out_content).append(emit)
                break
            emit, self._buf = self._buf[:idx], self._buf[idx + len(tag):]
            (out_reasoning if self._in_think else out_content).append(emit)
            self._in_think = not self._in_think
        return "".join(out_content), "".join(out_reasoning)

    def flush(self) -> tuple[str, str]:
        """Call once the stream ends: any still-buffered text (e.g. an
        unclosed `<think>` tag left dangling by a truncated/cancelled turn)
        is emitted rather than silently dropped."""
        emit, self._buf = self._buf, ""
        if not emit:
            return "", ""
        return ("", emit) if self._in_think else (emit, "")


def _split_think_tags(delta: Delta, splitter: ThinkTagSplitter) -> Delta | None:
    """Route a chunk's `content` through `splitter`, merging any extracted
    reasoning into `delta.reasoning`. Returns `None` when the split leaves
    nothing worth emitting (a chunk that was purely a `<think>`/`</think>`
    marker with no other content and no finish/usage signal)."""
    if delta.content is None:
        return delta
    content, reasoning = splitter.feed(delta.content)
    merged_reasoning = f"{delta.reasoning}{reasoning}" if delta.reasoning else (reasoning or None)
    if not content and not merged_reasoning and delta.finish_reason is None and delta.usage is None:
        return None
    return delta.model_copy(update={"content": content or None, "reasoning": merged_reasoning})


class OpenAICompatibleProvider(Provider):
    DEFAULT_MODEL = "gpt-4o"

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
            base_url=config.base_url.rstrip("/"),
            timeout=httpx.Timeout(60.0, connect=3.0, read=60.0),
            headers={"Authorization": f"Bearer {api_key}"} if api_key else {},
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
            "messages": [self._serialize_message(m) for m in messages],
            "stream": True,
            # asks the server to append a final chunk carrying token usage
            "stream_options": {"include_usage": True},
        }
        if tools:
            body["tools"] = tools
        if temperature is not None:
            body["temperature"] = temperature
        if top_p is not None:
            body["top_p"] = top_p
        if max_tokens is not None:
            body["max_tokens"] = max_tokens

        # Streamed tool calls arrive as fragments (an index plus partial
        # `arguments` strings); they must be accumulated per index and only
        # emitted as complete ToolCalls once the stream finishes.
        tool_call_buf: dict[int, dict[str, str]] = {}
        think_splitter = ThinkTagSplitter()

        try:
            async with self._client.stream("POST", "/v1/chat/completions", json=body) as response:
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
                    if line.startswith("data: "):
                        data_str = line[6:]
                        if data_str == "[DONE]":
                            break
                        delta = self._parse_chunk(data_str, tool_call_buf)
                        if delta:
                            delta = _split_think_tags(delta, think_splitter)
                        if delta:
                            yield delta
            trailing_content, trailing_reasoning = think_splitter.flush()
            if trailing_content or trailing_reasoning:
                yield Delta(content=trailing_content or None, reasoning=trailing_reasoning or None)
            final_calls = self._assemble_tool_calls(tool_call_buf)
            if final_calls:
                yield Delta(tool_calls=final_calls)
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
                content=f"[Provider connection error: {e}]",
                finish_reason="error",
            )

    def _serialize_message(self, msg: Message) -> dict[str, Any]:
        content: Any = msg.content or ""
        if isinstance(msg.content, list):
            content = self._blocks_to_openai(msg.content)
        base: dict[str, Any] = {"role": msg.role.value, "content": content}
        if msg.tool_calls:
            base["tool_calls"] = msg.tool_calls
        if msg.role.value == "tool" and msg.tool_call_id:
            base["tool_call_id"] = msg.tool_call_id
        return base

    @staticmethod
    def _blocks_to_openai(blocks: list[dict[str, Any]]) -> list[dict[str, Any]]:
        parts: list[dict[str, Any]] = []
        for b in blocks:
            if b.get("type") == "text":
                parts.append({"type": "text", "text": b.get("text", "")})
            elif b.get("type") == "image":
                mime = b.get("mime_type", "image/png")
                parts.append({
                    "type": "image_url",
                    "image_url": {"url": f"data:{mime};base64,{b.get('data', '')}"},
                })
        return parts

    def _parse_chunk(
        self,
        data_str: str,
        tool_call_buf: dict[int, dict[str, str]],
    ) -> Delta | None:
        try:
            chunk = json.loads(data_str)
        except json.JSONDecodeError:
            return None

        # The usage chunk (from stream_options.include_usage) has no choices
        usage_data = chunk.get("usage")
        usage = None
        if usage_data:
            usage = {
                "input_tokens": int(usage_data.get("prompt_tokens") or 0),
                "output_tokens": int(usage_data.get("completion_tokens") or 0),
            }

        choices = chunk.get("choices", [])
        if not choices:
            return Delta(usage=usage) if usage else None

        choice = choices[0]
        delta_data = choice.get("delta", {})

        for tc in delta_data.get("tool_calls") or []:
            idx = tc.get("index", 0)
            buf = tool_call_buf.setdefault(
                idx, {"id": "", "name": "", "arguments": ""}
            )
            if tc.get("id"):
                buf["id"] = tc["id"]
            fn = tc.get("function") or {}
            if fn.get("name"):
                buf["name"] = fn["name"]
            if fn.get("arguments"):
                buf["arguments"] += fn["arguments"]

        content = delta_data.get("content")
        # DeepSeek/vLLM-style reasoning channel; some servers use "reasoning"
        reasoning = delta_data.get("reasoning_content") or delta_data.get("reasoning")
        finish_reason = choice.get("finish_reason")
        if content is None and reasoning is None and finish_reason is None and usage is None:
            return None

        return Delta(
            role=delta_data.get("role") or "assistant",
            content=content,
            reasoning=reasoning,
            finish_reason=finish_reason,
            usage=usage,
        )

    @staticmethod
    def _assemble_tool_calls(
        tool_call_buf: dict[int, dict[str, str]],
    ) -> list[ToolCall] | None:
        if not tool_call_buf:
            return None
        return [
            ToolCall(
                id=buf["id"],
                type="function",
                function={"name": buf["name"], "arguments": buf["arguments"]},
            )
            for _, buf in sorted(tool_call_buf.items())
        ]

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
                name=item.get("id"),
                provider_id=self.provider_id,
            ))
        return models

    async def count_tokens(self, messages: list[dict]) -> int:
        enc = _get_encoding(self.resolve_model())
        total = 0
        for msg in messages:
            total += len(enc.encode(str(msg.get("content", ""))))
            total += 4
        total += 2
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
