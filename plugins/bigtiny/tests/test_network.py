"""Unit tests for Tailscale-aware routing (bigtiny/network.py).
Run: pytest tests/test_network.py -v
"""

from __future__ import annotations

import httpx
import pytest

from bigtiny.network import (
    TAILSCALE_NETWORK,
    PreferDirectTransport,
    TailscaleClient,
    is_tailscale_ip,
)


# ---------------------------------------------------------------------------
# is_tailscale_ip
# ---------------------------------------------------------------------------

def test_is_tailscale_ip_identifies_range_boundaries():
    assert is_tailscale_ip("100.64.0.0")
    assert is_tailscale_ip("100.100.50.25")
    assert is_tailscale_ip("100.127.255.255")


def test_is_tailscale_ip_false_for_rfc1918_addresses():
    assert not is_tailscale_ip("192.168.1.5")
    assert not is_tailscale_ip("10.0.0.1")
    assert not is_tailscale_ip("172.16.0.1")


def test_is_tailscale_ip_false_for_addresses_just_outside_the_range():
    assert not is_tailscale_ip("100.63.255.255")
    assert not is_tailscale_ip("100.128.0.0")


def test_is_tailscale_ip_false_for_non_ip_hostnames():
    assert not is_tailscale_ip("example.com")
    assert not is_tailscale_ip("localhost")
    assert not is_tailscale_ip("")


# ---------------------------------------------------------------------------
# TailscaleClient.get_peers
# ---------------------------------------------------------------------------

_SAMPLE_STATUS = {
    "Peer": {
        "abc123": {
            "DNSName": "myserver.tail1234.ts.net.",
            "TailscaleIPs": ["100.90.1.2", "fd7a:115c:a1e0::1"],
        },
        "def456": {
            "DNSName": "laptop.tail1234.ts.net.",
            "TailscaleIPs": ["100.90.1.3"],
        },
    }
}


@pytest.mark.asyncio
async def test_get_peers_parses_api_response():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=_SAMPLE_STATUS)

    client = TailscaleClient(_transport=httpx.MockTransport(handler))
    peers = await client.get_peers()
    assert peers["100.90.1.2"].name == "myserver.tail1234.ts.net"
    assert peers["100.90.1.3"].name == "laptop.tail1234.ts.net"
    assert "fd7a:115c:a1e0::1" in peers  # IPv6 Tailscale address also indexed


@pytest.mark.asyncio
async def test_get_peers_handles_daemon_not_running():
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("connection refused", request=request)

    client = TailscaleClient(_transport=httpx.MockTransport(handler))
    peers = await client.get_peers()
    assert peers == {}


@pytest.mark.asyncio
async def test_get_peers_caches_across_calls():
    calls = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        calls["n"] += 1
        return httpx.Response(200, json=_SAMPLE_STATUS)

    client = TailscaleClient(_transport=httpx.MockTransport(handler))
    await client.get_peers()
    await client.get_peers()
    assert calls["n"] == 1


# ---------------------------------------------------------------------------
# TailscaleClient.resolve_direct_ip
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_resolve_direct_ip_returns_direct_address_for_known_peer(monkeypatch):
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=_SAMPLE_STATUS)

    client = TailscaleClient(_transport=httpx.MockTransport(handler))
    monkeypatch.setattr(
        TailscaleClient,
        "_resolve_dns_excluding_tailscale",
        staticmethod(lambda hostname: _fake_resolve(hostname)),
    )
    direct = await client.resolve_direct_ip("100.90.1.2")
    assert direct == "192.168.1.50"


async def _fake_resolve(hostname: str) -> str | None:
    if hostname == "myserver.tail1234.ts.net":
        return "192.168.1.50"
    return None


@pytest.mark.asyncio
async def test_resolve_direct_ip_returns_none_for_unknown_peer():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json=_SAMPLE_STATUS)

    client = TailscaleClient(_transport=httpx.MockTransport(handler))
    direct = await client.resolve_direct_ip("100.90.9.9")
    assert direct is None


@pytest.mark.asyncio
async def test_resolve_dns_excluding_tailscale_returns_a_non_tailscale_address():
    # Real DNS resolution against "localhost" — no mocking needed, and it
    # exercises the actual exclude-Tailscale-addresses filter.
    result = await TailscaleClient._resolve_dns_excluding_tailscale("localhost")
    assert result is not None
    assert not is_tailscale_ip(result)


@pytest.mark.asyncio
async def test_resolve_dns_excluding_tailscale_returns_none_for_bogus_hostname():
    result = await TailscaleClient._resolve_dns_excluding_tailscale(
        "this-hostname-should-not-exist.invalid"
    )
    assert result is None


# ---------------------------------------------------------------------------
# PreferDirectTransport
# ---------------------------------------------------------------------------

class _StubTailscale:
    def __init__(self, direct_ip: str | None):
        self._direct_ip = direct_ip

    async def resolve_direct_ip(self, host: str) -> str | None:
        return self._direct_ip


@pytest.mark.asyncio
async def test_prefer_direct_transport_tries_direct_first():
    seen_hosts = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen_hosts.append(request.url.host)
        return httpx.Response(200, text="ok")

    transport = PreferDirectTransport(
        _StubTailscale("192.168.1.50"), inner=httpx.MockTransport(handler)
    )
    async with httpx.AsyncClient(transport=transport) as client:
        resp = await client.get("http://100.90.1.2/health")
    assert resp.status_code == 200
    assert seen_hosts == ["192.168.1.50"]


@pytest.mark.asyncio
async def test_prefer_direct_transport_falls_back_to_tailscale_on_connect_error():
    seen_hosts = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen_hosts.append(request.url.host)
        if request.url.host == "192.168.1.50":
            raise httpx.ConnectError("unreachable", request=request)
        return httpx.Response(200, text="ok")

    transport = PreferDirectTransport(
        _StubTailscale("192.168.1.50"), inner=httpx.MockTransport(handler)
    )
    async with httpx.AsyncClient(transport=transport) as client:
        resp = await client.get("http://100.90.1.2/health")
    assert resp.status_code == 200
    assert seen_hosts == ["192.168.1.50", "100.90.1.2"]


@pytest.mark.asyncio
async def test_prefer_direct_transport_skips_localhost():
    seen_hosts = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen_hosts.append(request.url.host)
        return httpx.Response(200, text="ok")

    # A tailscale client that would resolve a direct IP if asked — but it
    # must never be asked for a localhost request.
    transport = PreferDirectTransport(
        _StubTailscale("192.168.1.50"), inner=httpx.MockTransport(handler)
    )
    async with httpx.AsyncClient(transport=transport) as client:
        resp = await client.get("http://127.0.0.1:8080/health")
    assert resp.status_code == 200
    assert seen_hosts == ["127.0.0.1"]


@pytest.mark.asyncio
async def test_prefer_direct_transport_noop_for_non_tailscale_host():
    seen_hosts = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen_hosts.append(request.url.host)
        return httpx.Response(200, text="ok")

    transport = PreferDirectTransport(
        _StubTailscale("192.168.1.50"), inner=httpx.MockTransport(handler)
    )
    async with httpx.AsyncClient(transport=transport) as client:
        resp = await client.get("https://api.openai.com/v1/models")
    assert resp.status_code == 200
    assert seen_hosts == ["api.openai.com"]


@pytest.mark.asyncio
async def test_prefer_direct_transport_noop_when_no_direct_address_known():
    seen_hosts = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen_hosts.append(request.url.host)
        return httpx.Response(200, text="ok")

    transport = PreferDirectTransport(_StubTailscale(None), inner=httpx.MockTransport(handler))
    async with httpx.AsyncClient(transport=transport) as client:
        resp = await client.get("http://100.90.1.2/health")
    assert resp.status_code == 200
    assert seen_hosts == ["100.90.1.2"]
