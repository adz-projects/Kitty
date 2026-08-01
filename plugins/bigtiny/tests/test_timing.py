"""Unit tests for per-LLM-call connection timing
(bigtiny/agent/loop.py's TimingResult / stream_with_timing). Run:
pytest tests/test_timing.py -v
"""

from __future__ import annotations

import asyncio
from typing import AsyncIterator

import pytest

from bigtiny.agent.loop import TimingResult, stream_with_timing
from bigtiny.providers.base import Delta


async def _deltas(items: list[Delta], delay: float = 0.001) -> AsyncIterator[Delta]:
    for d in items:
        if delay:
            await asyncio.sleep(delay)
        yield d


@pytest.mark.asyncio
async def test_stream_with_timing_captures_positive_metrics():
    timing = TimingResult()
    deltas = [
        Delta(content="hel"),
        Delta(content="lo"),
        Delta(finish_reason="stop", usage={"input_tokens": 10, "output_tokens": 2}),
    ]
    out = [d async for d in stream_with_timing(_deltas(deltas), timing)]
    assert len(out) == 3
    assert timing.ttfb_ms > 0
    assert timing.ttft_ms > 0
    assert timing.generation_ms > 0
    assert timing.total_tokens == 2


@pytest.mark.asyncio
async def test_stream_with_timing_monotonic_ordering():
    timing = TimingResult()
    deltas = [Delta(content="a"), Delta(content="b"), Delta(finish_reason="stop")]
    async for _ in stream_with_timing(_deltas(deltas), timing):
        pass
    assert timing.generation_ms >= timing.ttft_ms >= timing.ttfb_ms >= 0


@pytest.mark.asyncio
async def test_stream_with_timing_empty_stream_all_zero():
    timing = TimingResult()
    async for _ in stream_with_timing(_deltas([]), timing):
        pass
    assert timing.ttfb_ms == 0.0
    assert timing.ttft_ms == 0.0
    # generation_ms is set even for an empty stream (the loop just never runs).
    assert timing.generation_ms >= 0.0
    assert timing.total_tokens == 0


@pytest.mark.asyncio
async def test_stream_with_timing_error_only_stream_sets_ttfb_not_ttft():
    timing = TimingResult()
    deltas = [Delta(content="Provider error", finish_reason="error", error_type="other")]
    async for _ in stream_with_timing(_deltas(deltas), timing):
        pass
    # An error delta's `content` is a human-readable message, not model
    # output — TTFB reflects "the server responded", but TTFT must stay 0
    # since no real generation token was produced.
    assert timing.ttfb_ms > 0
    assert timing.ttft_ms == 0.0


@pytest.mark.asyncio
async def test_stream_with_timing_reasoning_only_first_delta_does_not_set_ttft():
    timing = TimingResult()
    deltas = [
        Delta(reasoning="thinking..."),
        Delta(reasoning="still thinking..."),
        Delta(content="the answer"),
        Delta(finish_reason="stop"),
    ]
    async for _ in stream_with_timing(_deltas(deltas), timing):
        pass
    assert timing.ttfb_ms > 0
    assert timing.ttft_ms > 0
    # ttft must be measured from the content delta, later than ttfb (which
    # was set on the first, reasoning-only delta).
    assert timing.ttft_ms > timing.ttfb_ms


@pytest.mark.asyncio
async def test_stream_with_timing_total_tokens_from_usage_delta():
    timing = TimingResult()
    deltas = [
        Delta(content="hi"),
        Delta(usage={"input_tokens": 5, "output_tokens": 7}),
        Delta(finish_reason="stop"),
    ]
    async for _ in stream_with_timing(_deltas(deltas), timing):
        pass
    assert timing.total_tokens == 7
