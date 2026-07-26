from __future__ import annotations

import asyncio
import json
import logging
import time
from typing import Any
from uuid import uuid4

import httpx
from bigtiny.models.mcp_server import (
    MCPServerConfig,
    ToolDefinition,
    ToolResult,
    TransportType,
)
from bigtiny.mcp.tools import truncate_output, validate_tool_args
from bigtiny.storage import Database

logger = logging.getLogger(__name__)

MCP_PROTOCOL_VERSION = "2024-11-05"
CLIENT_INFO = {"name": "bigtiny", "version": "0.1.0"}


class MCPServerError(Exception):
    pass


class MCPServerClient:
    def __init__(self, config: MCPServerConfig):
        self.config = config
        self._process: asyncio.subprocess.Process | None = None
        self._http_client: httpx.AsyncClient | None = None
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._request_id = 0
        # Demultiplexes stdio responses by JSON-RPC id so concurrent tool
        # calls against the same stdio server don't race reading each other's
        # response line off the one shared stdout stream (each caller used to
        # run its own read loop directly on `self._reader` — fine when calls
        # were always sequential, silently corrupting when they weren't).
        self._pending_stdio: dict[int, asyncio.Future] = {}
        self._stdio_reader_task: asyncio.Task | None = None
        self._tools: list[ToolDefinition] = []
        self._server_id = config.id or uuid4().hex[:8]
        self._sse_endpoint: str | None = None
        self._sse_event_stream: asyncio.Queue | None = None
        self._listener_task: asyncio.Task | None = None
        self._streamable_url: str | None = None
        # Some (non-stateless) streamable-http servers hand back a session id
        # on `initialize` that must be echoed on every later request — see
        # https://modelcontextprotocol.io/specification (Streamable HTTP
        # transport, session management). `None` until/unless the server
        # actually sends one; a stateless server (confirmed observed with a
        # real deployment) never sets it and every request works standalone.
        self._mcp_session_id: str | None = None

    async def initialize(self) -> None:
        if self.config.transport == TransportType.stdio:
            await self._init_stdio()
        elif self.config.transport == TransportType.sse:
            await self._init_sse()
        elif self.config.transport == TransportType.streamable_http:
            await self._init_streamable_http()
        else:
            raise MCPServerError(f"Unsupported transport: {self.config.transport}")

        result = await self._send_request("initialize", {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": CLIENT_INFO,
        })
        logger.info("MCP server %s initialized: %s", self._server_id, result)

        await self._send_notification("notifications/initialized", {})

        tools_result = await self._send_request("tools/list", {})
        self._tools = [
            ToolDefinition(
                name=t["name"],
                description=t.get("description", ""),
                input_schema=t.get("inputSchema", {}),
                server_id=self._server_id,
            )
            for t in tools_result.get("tools", [])
        ]
        logger.info(
            "Discovered %d tools from MCP server %s",
            len(self._tools), self._server_id,
        )

    async def _init_stdio(self) -> None:
        cmd = self.config.command or ""
        args_list = self.config.args or []
        env = self.config.env or {}

        full_env = {**__import__("os").environ, **env}

        try:
            self._process = await asyncio.create_subprocess_exec(
                cmd,
                *args_list,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=full_env,
            )
        except NotImplementedError:
            raise MCPServerError(
                "This event loop cannot spawn subprocesses (Windows "
                "SelectorEventLoop). Run the daemon via `python -m bigtiny` "
                "so the Proactor loop factory is used."
            )
        if self._process.stdout is None:
            raise MCPServerError("MCP process stdout not available")
        if self._process.stdin is None:
            raise MCPServerError("MCP process stdin not available")

        self._reader = self._process.stdout
        self._writer = self._process.stdin
        self._stdio_reader_task = asyncio.create_task(self._stdio_reader_loop())

    async def _init_sse(self) -> None:
        self._http_client = httpx.AsyncClient(
            timeout=httpx.Timeout(30.0), headers=self.config.headers or {}
        )
        url = self.config.url or ""
        self._sse_endpoint = url
        self._sse_event_stream = asyncio.Queue()
        self._listener_task = asyncio.create_task(self._sse_listener())

    async def _init_streamable_http(self) -> None:
        self._http_client = httpx.AsyncClient(
            timeout=httpx.Timeout(30.0), headers=self.config.headers or {}
        )
        self._streamable_url = self.config.url or ""

    async def _sse_listener(self) -> None:
        if self._sse_endpoint is None or self._http_client is None or self._sse_event_stream is None:
            logger.error("SSE listener for %s called before _init_sse", self._server_id)
            return
        try:
            async with self._http_client.stream("GET", self._sse_endpoint) as response:
                response.raise_for_status()
                current_event = ""
                async for line in response.aiter_lines():
                    line = line.strip()
                    if line.startswith("event: "):
                        current_event = line[7:]
                    elif line.startswith("data: "):
                        data = line[6:]
                        await self._sse_event_stream.put({
                            "event": current_event,
                            "data": data,
                        })
                    elif not line:
                        current_event = ""
        except Exception as e:
            logger.error("SSE listener for %s failed: %s", self._server_id, e)

    async def _read_stdio_line(self) -> str:
        if self._reader is None:
            raise MCPServerError("Cannot read: MCP stdio not initialized")
        line = await self._reader.readline()
        return line.decode("utf-8").strip()

    async def _stdio_reader_loop(self) -> None:
        """Single reader for the whole stdio connection's lifetime — the only
        coroutine that ever calls `self._reader.readline()`. Every in-flight
        `_send_stdio_request` registers a future in `self._pending_stdio`
        keyed by request id and just awaits it; this loop is what resolves
        those futures as responses arrive, in whatever order they arrive,
        which is what makes concurrent requests to one stdio server safe."""
        try:
            while True:
                line = await self._read_stdio_line()
                if not line:
                    break  # EOF: the process closed stdout (exited or crashed)
                try:
                    response = json.loads(line)
                except json.JSONDecodeError:
                    # Stray non-JSON-RPC line on stdout — ignore and keep reading.
                    continue
                resp_id = response.get("id")
                fut = self._pending_stdio.pop(resp_id, None) if resp_id is not None else None
                if fut is None or fut.done():
                    continue  # not a response we're waiting on (or already resolved)
                if "error" in response:
                    err = response["error"]
                    fut.set_exception(
                        MCPServerError(f"MCP error {err.get('code')}: {err.get('message')}")
                    )
                else:
                    fut.set_result(response.get("result", {}))
        except asyncio.CancelledError:
            raise
        except Exception as e:
            logger.error("stdio reader loop for %s failed: %s", self._server_id, e)
        finally:
            # The process died (or the loop errored) with requests still
            # in flight — wake them all with an error instead of hanging
            # forever on a future nothing will ever resolve.
            for fut in self._pending_stdio.values():
                if not fut.done():
                    fut.set_exception(MCPServerError("MCP stdio connection closed"))
            self._pending_stdio.clear()

    async def _send_notification(self, method: str, params: dict[str, Any]) -> None:
        notification = {
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }

        if self.config.transport == TransportType.stdio:
            if self._writer is None:
                raise MCPServerError("Cannot write: MCP stdio not initialized")
            line = json.dumps(notification) + "\n"
            self._writer.write(line.encode("utf-8"))
            await self._writer.drain()
        elif self.config.transport == TransportType.sse:
            if self._http_client is None or self._sse_endpoint is None:
                raise MCPServerError("MCP SSE transport not initialized")
            session_url = self._sse_endpoint.replace("/sse", "/message")
            await self._http_client.post(session_url, json=notification)
        elif self.config.transport == TransportType.streamable_http:
            await self._send_streamable_http_notification(notification)
        else:
            raise MCPServerError(f"Unsupported transport: {self.config.transport}")

    async def _send_request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._request_id += 1
        request = {
            "jsonrpc": "2.0",
            "id": self._request_id,
            "method": method,
            "params": params,
        }

        if self.config.transport == TransportType.stdio:
            return await self._send_stdio_request(request)
        elif self.config.transport == TransportType.sse:
            return await self._send_sse_request(request)
        elif self.config.transport == TransportType.streamable_http:
            return await self._send_streamable_http_request(request)
        else:
            raise MCPServerError(f"Unsupported transport: {self.config.transport}")

    async def _send_stdio_request(self, request: dict) -> dict[str, Any]:
        if self._writer is None:
            raise MCPServerError("Cannot write: MCP stdio not initialized")
        req_id = request["id"]
        fut: asyncio.Future = asyncio.get_event_loop().create_future()
        self._pending_stdio[req_id] = fut
        try:
            line = json.dumps(request) + "\n"
            self._writer.write(line.encode("utf-8"))
            await self._writer.drain()
            return await fut
        finally:
            # If we timed out/were cancelled waiting, don't leave a stale
            # entry for a response that arrives later to match against.
            self._pending_stdio.pop(req_id, None)

    async def _send_sse_request(self, request: dict) -> dict[str, Any]:
        if self._http_client is None or self._sse_endpoint is None:
            raise MCPServerError("MCP SSE transport not initialized")
        session_url = self._sse_endpoint.replace("/sse", "/message")
        response = await self._http_client.post(
            session_url,
            json=request,
        )
        response.raise_for_status()
        data = response.json()
        if "error" in data:
            err = data["error"]
            raise MCPServerError(f"MCP error {err.get('code')}: {err.get('message')}")
        return data.get("result", {})

    def _streamable_http_headers(self) -> dict[str, str]:
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if self._mcp_session_id:
            headers["Mcp-Session-Id"] = self._mcp_session_id
        return headers

    def _capture_session_id(self, response: httpx.Response) -> None:
        session_id = response.headers.get("mcp-session-id")
        if session_id:
            self._mcp_session_id = session_id

    async def _send_streamable_http_notification(self, notification: dict) -> None:
        if self._http_client is None or self._streamable_url is None:
            raise MCPServerError("MCP streamable-http transport not initialized")
        response = await self._http_client.post(
            self._streamable_url, json=notification, headers=self._streamable_http_headers()
        )
        response.raise_for_status()
        self._capture_session_id(response)
        # A notification has no id and gets no JSON-RPC response — servers
        # reply 202 Accepted with an empty body; there is nothing to parse.

    async def _send_streamable_http_request(self, request: dict) -> dict[str, Any]:
        if self._http_client is None or self._streamable_url is None:
            raise MCPServerError("MCP streamable-http transport not initialized")
        response = await self._http_client.post(
            self._streamable_url, json=request, headers=self._streamable_http_headers()
        )
        response.raise_for_status()
        self._capture_session_id(response)

        content_type = response.headers.get("content-type", "")
        if "text/event-stream" in content_type:
            data = self._parse_sse_body(response.text)
            if data is None:
                raise MCPServerError("streamable-http response had no data frame")
        else:
            data = response.json()

        if "error" in data:
            err = data["error"]
            raise MCPServerError(f"MCP error {err.get('code')}: {err.get('message')}")
        return data.get("result", {})

    @staticmethod
    def _parse_sse_body(text: str) -> dict[str, Any] | None:
        """First `data:` line of a Streamable HTTP response body, parsed as
        JSON — a single POST here always gets exactly one JSON-RPC reply, so
        (unlike a genuine long-lived SSE stream) there's only ever one frame
        worth reading."""
        for line in text.splitlines():
            line = line.strip()
            if line.startswith("data:"):
                payload = line[len("data:"):].strip()
                if payload:
                    return json.loads(payload)
        return None

    @property
    def tools(self) -> list[ToolDefinition]:
        return list(self._tools)

    async def execute_tool(
        self,
        tool_name: str,
        args: dict[str, Any],
        timeout: int = 30,
    ) -> ToolResult:
        tool_def = next((t for t in self._tools if t.name == tool_name), None)
        if not tool_def:
            # Returned, not raised: `execute_tool`'s documented/relied-upon
            # contract (see `Agent._run_one_tool_call`'s docstring in
            # loop.py, and the "tool_def not found" case in
            # `MCPManager.execute_tool` below, which already returns rather
            # than raises) is that it never raises — its caller's
            # `asyncio.gather` isn't given `return_exceptions=True`, so an
            # exception here would cancel every sibling concurrent tool
            # call in the turn instead of just failing this one.
            return ToolResult(
                content=f"[Unknown tool: {tool_name}]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=0,
                output_size_bytes=0,
                is_error=True,
            )

        try:
            validate_tool_args(tool_def, args)
        except ValueError as e:
            return ToolResult(
                content=f"[Invalid arguments for tool '{tool_name}': {e}]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=0,
                output_size_bytes=0,
                is_error=True,
            )

        start = time.monotonic()
        try:
            result = await asyncio.wait_for(
                self._send_request("tools/call", {
                    "name": tool_name,
                    "arguments": args,
                }),
                timeout=timeout,
            )
            duration = int((time.monotonic() - start) * 1000)

            raw_content = self._extract_content(result)
            content, truncated = truncate_output(raw_content)

            return ToolResult(
                content=content,
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=duration,
                output_size_bytes=len(raw_content.encode("utf-8")),
                is_error=False,
                truncated=truncated,
            )
        except asyncio.TimeoutError:
            duration = int((time.monotonic() - start) * 1000)
            return ToolResult(
                content=f"[Tool '{tool_name}' timed out after {timeout}s]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=duration,
                output_size_bytes=0,
                is_error=True,
            )
        except MCPServerError as e:
            duration = int((time.monotonic() - start) * 1000)
            return ToolResult(
                content=f"[Tool '{tool_name}' error: {e}]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=duration,
                output_size_bytes=0,
                is_error=True,
            )
        except Exception as e:
            # Catches anything `_send_request`/`_extract_content`/
            # `truncate_output` could raise beyond the two expected cases
            # above (a dropped connection, a malformed MCP response
            # tripping a bug in `_extract_content`, etc.) — without this,
            # an unusual failure here would propagate uncaught through
            # `MCPManager.execute_tool` and `Agent._run_one_tool_call`
            # into the turn's `asyncio.gather` (not given
            # `return_exceptions=True`), killing every concurrent tool
            # call in the turn instead of just this one.
            duration = int((time.monotonic() - start) * 1000)
            logger.exception("Unexpected error executing tool '%s'", tool_name)
            return ToolResult(
                content=f"[Tool '{tool_name}' failed unexpectedly: {e}]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=duration,
                output_size_bytes=0,
                is_error=True,
            )

    def _extract_content(self, result: dict[str, Any]) -> str:
        content_parts = result.get("content", [])
        texts = []
        for part in content_parts:
            if isinstance(part, dict):
                if part.get("type") == "text":
                    texts.append(part.get("text", ""))
                elif part.get("type") == "resource":
                    texts.append(str(part.get("resource", "")))
            elif isinstance(part, str):
                texts.append(part)
        return "\n".join(texts)

    async def shutdown(self) -> None:
        if self._listener_task:
            self._listener_task.cancel()
            try:
                await self._listener_task
            except asyncio.CancelledError:
                pass
            self._listener_task = None

        if self._stdio_reader_task:
            self._stdio_reader_task.cancel()
            try:
                await self._stdio_reader_task
            except asyncio.CancelledError:
                pass
            self._stdio_reader_task = None

        if self._writer:
            try:
                self._writer.close()
            except Exception:
                pass
            self._writer = None

        if self._process:
            try:
                self._process.terminate()
                await asyncio.wait_for(self._process.wait(), timeout=5)
            except asyncio.TimeoutError:
                self._process.kill()
                await self._process.wait()
            except Exception:
                pass
            self._process = None

        if self._http_client:
            await self._http_client.aclose()
            self._http_client = None


class MCPManager:
    def __init__(self, db: Database):
        self.db = db
        self._servers: dict[str, MCPServerClient] = {}
        self._tool_registry: dict[str, ToolDefinition] = {}

    async def connect_server(self, server_id: str) -> str:
        row = await self.db.fetch_one(
            "SELECT * FROM mcp_servers WHERE id = :id", {"id": server_id}
        )
        if not row:
            raise MCPServerError(f"MCP server not found: {server_id}")

        config = MCPServerConfig(
            id=row["id"],
            name=row["name"],
            transport=TransportType(row["transport"]),
            command=row.get("command"),
            url=row.get("url"),
        )
        if row.get("args"):
            config.args = json.loads(row["args"])
        if row.get("env"):
            config.env = json.loads(row["env"])
        if row.get("headers"):
            config.headers = json.loads(row["headers"])

        client = MCPServerClient(config)
        try:
            await asyncio.wait_for(client.initialize(), timeout=60)
        except asyncio.TimeoutError:
            err_msg = f"MCP server '{server_id}' initialization timed out after 60s"
            logger.error(err_msg)
            await self.db.execute(
                "UPDATE mcp_servers SET status = 'error', error_message = :err WHERE id = :id",
                {"id": server_id, "err": err_msg},
            )
            raise MCPServerError(err_msg)
        except Exception as e:
            logger.error("MCP server %s init failed: %s: %s", server_id, type(e).__name__, str(e) or repr(e))
            stderr_text = ""
            if client._process and client._process.stderr:
                try:
                    stderr_text = (
                        await asyncio.wait_for(
                            client._process.stderr.read(), timeout=2
                        )
                    ).decode("utf-8", errors="replace")
                except Exception:
                    pass
            if stderr_text:
                logger.error("MCP server %s stderr:\n%s", server_id, stderr_text)
            await self.db.execute(
                "UPDATE mcp_servers SET status = 'error', error_message = :err WHERE id = :id",
                {"id": server_id, "err": str(e)},
            )
            raise

        self._servers[server_id] = client
        for tool in client.tools:
            self._tool_registry[tool.name] = tool

        await self.db.execute(
            "UPDATE mcp_servers SET status = 'connected' WHERE id = :id",
            {"id": server_id},
        )
        return "connected"

    async def connect_all(self) -> None:
        servers = await self.db.fetch_all(
            "SELECT * FROM mcp_servers WHERE COALESCE(enabled, 1) = 1"
        )

        async def _connect_one(server_id: str) -> None:
            try:
                await self.connect_server(server_id)
            except Exception as e:
                logger.warning("Failed to connect MCP server %s: %s", server_id, e)

        # Connecting servers sequentially meant one slow/unreachable server
        # (each with its own 60s `connect_server` timeout, see below) added
        # up to 60s of daemon startup latency per bad server before the
        # next one even began connecting. Each server already carries its
        # own try/except (`_connect_one`), so concurrency here can't turn
        # one server's failure into a startup-wide failure.
        await asyncio.gather(*[_connect_one(s["id"]) for s in servers])

    async def list_tools(self, server_id: str | None = None) -> list[ToolDefinition]:
        if server_id:
            client = self._servers.get(server_id)
            if not client:
                return []
            return client.tools
        return list(self._tool_registry.values())

    async def execute_tool(
        self,
        tool_name: str,
        args: dict[str, Any],
        timeout: int = 30,
    ) -> ToolResult:
        tool_def = self._tool_registry.get(tool_name)
        if not tool_def:
            return ToolResult(
                content=f"[Unknown tool: {tool_name}]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=0,
                output_size_bytes=0,
                is_error=True,
            )

        client = self._servers.get(tool_def.server_id)
        if not client:
            return ToolResult(
                content=f"[Server for tool '{tool_name}' is not connected]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=0,
                output_size_bytes=0,
                is_error=True,
            )

        try:
            return await client.execute_tool(tool_name, args, timeout)
        except Exception as e:
            # Defense in depth: `MCPServerClient.execute_tool` is written to
            # never raise (see its own broad except), but this dispatch
            # point is the one thing every tool call in the daemon funnels
            # through (`Agent._run_one_tool_call` in loop.py relies on
            # never seeing an exception here — its caller's `asyncio.gather`
            # isn't given `return_exceptions=True`, so one escaping
            # exception would otherwise cancel every concurrent tool call
            # in the turn), so it's worth not depending solely on that
            # invariant holding inside every current and future transport.
            logger.exception("Unexpected error dispatching tool '%s'", tool_name)
            return ToolResult(
                content=f"[Tool '{tool_name}' failed unexpectedly: {e}]",
                tool_call_id=f"{tool_name}_{uuid4().hex[:8]}",
                duration_ms=0,
                output_size_bytes=0,
                is_error=True,
            )

    async def disconnect_server(self, server_id: str) -> None:
        client = self._servers.pop(server_id, None)
        if client:
            for tool_name in list(self._tool_registry.keys()):
                if self._tool_registry[tool_name].server_id == server_id:
                    del self._tool_registry[tool_name]
            await client.shutdown()
            await self.db.execute(
                "UPDATE mcp_servers SET status = 'disconnected' WHERE id = :id",
                {"id": server_id},
            )

    async def disconnect_all(self) -> None:
        for server_id in list(self._servers.keys()):
            await self.disconnect_server(server_id)
