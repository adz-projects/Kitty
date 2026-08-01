from __future__ import annotations

import asyncio
import logging
import socket
from ipaddress import ip_address, ip_network
from typing import NamedTuple

import httpx

logger = logging.getLogger(__name__)

TAILSCALE_NETWORK = ip_network("100.64.0.0/10")
TAILSCALE_LOCAL_API = "http://[::1]:42711/localapi/v0/status?peers=1"

# How long a direct-address attempt gets before falling back to the
# Tailscale tunnel — deliberately short: a failed direct address should add
# minimal latency, not stall the request.
DIRECT_CONNECT_TIMEOUT_S = 3.0


def is_tailscale_ip(host: str) -> bool:
    """True if `host` parses as an IP address inside the Tailscale CGNAT
    range (100.64.0.0/10). Non-IP hosts (regular hostnames) and addresses
    outside the range (including ordinary RFC1918 LAN IPs) are False."""
    try:
        return ip_address(host) in TAILSCALE_NETWORK
    except ValueError:
        return False


class TailscalePeer(NamedTuple):
    name: str
    tailscale_ip: str
    direct_ips: tuple[str, ...]


class TailscaleClient:
    """Talks to the local Tailscale daemon's localapi to discover peers and
    resolve a peer's LAN-reachable address, if any, so a request to a
    Tailscale IP can try the direct path first instead of always going
    through the tunnel. Lazy: costs nothing at startup, and a daemon that
    isn't running (or doesn't exist) degrades to a silent no-op — every
    caller just falls back to the original Tailscale-IP request."""

    def __init__(self, _transport: httpx.AsyncBaseTransport | None = None) -> None:
        self._peers_by_ip: dict[str, TailscalePeer] | None = None
        self._resolved_cache: dict[str, str | None] = {}
        self._warned_unavailable = False
        self._lock = asyncio.Lock()
        # Test seam only — lets tests inject an httpx.MockTransport for the
        # local-API call instead of hitting a real Tailscale daemon.
        self._transport = _transport

    async def get_peers(self) -> dict[str, TailscalePeer]:
        async with self._lock:
            if self._peers_by_ip is not None:
                return self._peers_by_ip
            self._peers_by_ip = await self._fetch_peers()
            return self._peers_by_ip

    async def _fetch_peers(self) -> dict[str, TailscalePeer]:
        try:
            async with httpx.AsyncClient(
                timeout=httpx.Timeout(2.0), transport=self._transport
            ) as client:
                response = await client.get(TAILSCALE_LOCAL_API)
                response.raise_for_status()
                return self._parse_peers(response.json())
        except (httpx.HTTPError, ValueError):
            pass
        if not self._warned_unavailable:
            logger.warning(
                "Tailscale local API not reachable — direct-address routing disabled "
                "for Tailscale peers (this is a no-op if you don't use Tailscale)."
            )
            self._warned_unavailable = True
        return {}

    @staticmethod
    def _parse_peers(data: dict) -> dict[str, TailscalePeer]:
        peers: dict[str, TailscalePeer] = {}
        for peer in (data.get("Peer") or {}).values():
            name = peer.get("DNSName", "").rstrip(".")
            addrs = tuple(peer.get("TailscaleIPs") or [])
            if not name or not addrs:
                continue
            parsed = TailscalePeer(name=name, tailscale_ip=addrs[0], direct_ips=addrs)
            for ip in addrs:
                peers[ip] = parsed
        return peers

    async def resolve_direct_ip(self, host_or_ip: str) -> str | None:
        """If `host_or_ip` is a known Tailscale peer's IP, resolves that
        peer's DNS name via system DNS and returns the first non-Tailscale
        address it comes back with (e.g. a LAN IP, if the two machines share
        a network) — or None if no such address exists or the peer is
        unknown. Cached for the client's lifetime, refreshed only by
        constructing a new TailscaleClient (callers refresh on connection
        failure by simply retrying via the Tailscale IP, not by invalidating
        this cache — a resolution that was valid once rarely needs
        re-checking within a single daemon run)."""
        if host_or_ip in self._resolved_cache:
            return self._resolved_cache[host_or_ip]

        peers = await self.get_peers()
        peer = peers.get(host_or_ip)
        if peer is None:
            self._resolved_cache[host_or_ip] = None
            return None

        direct_ip = await self._resolve_dns_excluding_tailscale(peer.name)
        self._resolved_cache[host_or_ip] = direct_ip
        return direct_ip

    @staticmethod
    async def _resolve_dns_excluding_tailscale(hostname: str) -> str | None:
        try:
            loop = asyncio.get_running_loop()
            infos = await loop.getaddrinfo(hostname, None)
        except (socket.gaierror, OSError):
            return None
        for info in infos:
            candidate = info[4][0]
            if not is_tailscale_ip(candidate):
                return candidate
        return None


class PreferDirectTransport(httpx.AsyncBaseTransport):
    """Wraps the default httpx transport: for a request whose host is a
    Tailscale IP with a known direct (LAN) address, tries that address
    first and falls back to the original (tunneled) request on connection
    failure. A no-op for everything else — localhost, non-Tailscale hosts,
    or a Tailscale host with no discoverable direct address."""

    def __init__(self, tailscale: TailscaleClient, inner: httpx.AsyncBaseTransport | None = None):
        self._tailscale = tailscale
        self._inner = inner or httpx.AsyncHTTPTransport()

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        host = request.url.host
        if host in ("localhost", "127.0.0.1", "::1") or not is_tailscale_ip(host):
            return await self._inner.handle_async_request(request)

        direct_ip = await self._tailscale.resolve_direct_ip(host)
        if direct_ip is None:
            return await self._inner.handle_async_request(request)

        direct_url = request.url.copy_with(host=direct_ip)
        direct_request = httpx.Request(
            request.method,
            direct_url,
            headers=request.headers,
            stream=request.stream,
            extensions=request.extensions,
        )
        try:
            timeout = httpx.Timeout(DIRECT_CONNECT_TIMEOUT_S, connect=DIRECT_CONNECT_TIMEOUT_S)
            direct_request.extensions["timeout"] = timeout.as_dict()
            response = await self._inner.handle_async_request(direct_request)
            logger.debug("Using direct address %s for host %s (Tailscale IP)", direct_ip, host)
            return response
        except httpx.ConnectError:
            logger.info("Falling back to Tailscale for host %s (direct address unreachable)", host)
            return await self._inner.handle_async_request(request)

    async def aclose(self) -> None:
        await self._inner.aclose()
