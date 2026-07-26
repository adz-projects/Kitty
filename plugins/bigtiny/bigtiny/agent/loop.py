from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import re
from typing import Any, Awaitable, Callable

from bigtiny.storage import Database
from bigtiny.providers.router import ProviderRouter, NoHealthyProvider
from bigtiny.providers.base import ToolCall
from bigtiny.mcp.manager import MCPManager
from bigtiny.hitl.manager import HITLManager
from bigtiny.agent import sandbox
from bigtiny.agent.compaction import run_compaction
from bigtiny.agent.context_manager import ContextManager, SessionStats
from bigtiny.config import SummarizerConfig, TokenManagementConfig
from bigtiny.models.mcp_server import ToolDefinition
from bigtiny.models.session import Message, MessageRole
from bigtiny.providers.summarizer_client import SummarizerClient
from bigtiny.server.events import SSEEvent

logger = logging.getLogger(__name__)


def _dicts_to_messages(dicts: list[dict[str, Any]]) -> list[Message]:
    result = []
    for d in dicts:
        tc = d.get("tool_calls")
        result.append(Message(
            session_id="",
            role=MessageRole(d["role"]),
            content=d.get("content", ""),
            tool_calls=tc if tc else None,
            tool_call_id=d.get("tool_call_id"),
        ))
    return result


def _tools_to_openai_format(tools: list[ToolDefinition]) -> list[dict[str, Any]]:
    return [
        {
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            },
        }
        for t in tools
    ]


BUDGET_TOOL = {
    "type": "function",
    "function": {
        "name": "request_more_steps",
        "description": "Request additional steps to continue the current task",
        "parameters": {"type": "object", "properties": {}},
    },
}

# Client-side prompt-preamble wrappers Kitty prepends to the FIRST outgoing
# message of a session (a hidden system-prompt override, and — on a
# recipe-invoked first turn — a `<recipe>` wrapper around that). Mirrors the
# same patterns Kitty's own `stripPromptPreamble`/`stripRecipeWrapper`
# (src/stores/chat/errorUtils.ts) strip client-side for display: without this,
# `_derive_title` below would pick the wrapper's own first line ("<system>")
# as the session title instead of what the user actually typed.
_SYSTEM_WRAPPER_RE = re.compile(r"^<system>\n.*?\n</system>\n\n", re.DOTALL)
_RECIPE_WRAPPER_RE = re.compile(r"^<recipe\b[^>]*>\n.*?\n</recipe>\n\n[^\n]*\n\n", re.DOTALL)


def _strip_prompt_wrappers(text: str) -> str:
    """Best-effort: recover the real user-typed text from a wrapped first
    message, for title derivation only — not a security boundary, so a rare
    false-positive (a user message that happens to start with the exact
    wrapper shape) is an acceptable trade, same as the client-side version."""
    m = _RECIPE_WRAPPER_RE.match(text)
    if m:
        text = text[m.end():]
    m = _SYSTEM_WRAPPER_RE.match(text)
    if m:
        text = text[m.end():]
    return text


def _derive_title(text: str, max_len: int = 60) -> str:
    """First-line heuristic title for an untitled session."""
    text = _strip_prompt_wrappers(text)
    first_line = text.strip().splitlines()[0].strip() if text.strip() else ""
    if len(first_line) > max_len:
        first_line = first_line[:max_len].rsplit(" ", 1)[0] + "…"
    return first_line


BUDGET_SYSTEM_MESSAGE = (
    "[System: You have executed 20 steps. Summarize your progress, explain what "
    "remains, and call request_more_steps to continue.]"
)


class Agent:
    def __init__(
        self,
        router: ProviderRouter,
        mcp: MCPManager,
        hitl: HITLManager,
        context: ContextManager,
        db: Database,
        max_concurrent_tool_calls: int = 5,
        summarizer: SummarizerClient | None = None,
        token_management_config: TokenManagementConfig | None = None,
        summarizer_config: SummarizerConfig | None = None,
    ):
        self.router = router
        self.mcp = mcp
        self.hitl = hitl
        self.context = context
        self.db = db
        self.max_concurrent_tool_calls = max_concurrent_tool_calls
        self.summarizer = summarizer
        self.token_management_config = token_management_config or TokenManagementConfig()
        self.summarizer_config = summarizer_config or SummarizerConfig()
        self.stats = SessionStats(db)
        self._tasks: dict[str, asyncio.Task] = {}
        self._loop_history: dict[str, list[str]] = {}
        # Background compaction passes, tracked so `shutdown()` can cancel
        # any still in flight rather than leaving them to race a closing DB
        # connection during process exit.
        self._compaction_tasks: set[asyncio.Task] = set()
        # Keyed by `action_id`, not `session_id` — a single turn can have more
        # than one tool call pending approval at once (this becomes possible
        # once tool calls execute concurrently), and each needs its own,
        # independently-resolvable wait; a session-keyed dict could only ever
        # track the most recently registered wait, silently losing any others.
        self._hitl_events: dict[str, asyncio.Event] = {}

    async def run(
        self,
        session_id: str,
        user_message: str,
        event_callback: Callable[[SSEEvent], Awaitable[None]],
        provider_override: str | None = None,
        images: list[dict[str, str]] | None = None,
    ) -> None:
        # Per-run state: must not live on the instance, or concurrent
        # sessions would corrupt each other's budget counters.
        tool_call_count = 0
        self._loop_history[session_id] = []

        session = await self.db.fetch_one(
            "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
        )
        if not session:
            await event_callback(SSEEvent(
                type="error",
                error_message=f"Session {session_id} not found",
                session_id=session_id,
                is_last=True,
            ))
            return

        metadata = json.loads(session["metadata"]) if session.get("metadata") else {}
        persona_override = metadata.get("persona_override")
        # Per-session config (set via PATCH /api/chat/{id}/config); an
        # explicit provider_override argument still wins.
        provider_override = provider_override or metadata.get("provider")
        model_override = metadata.get("model")
        # Directory-sandboxing scope for every tool call this turn (mode-
        # dependent — see `sandbox.allowed_dirs_for_session`'s doc comment).
        # Computed once per run from the metadata snapshot fetched above,
        # which already reflects any "Set as working directory"/mode-switch
        # PATCH that landed before this turn started.
        allowed_dirs = sandbox.allowed_dirs_for_session(metadata, sandbox.CACHE_DIR)

        active_tools = await self.mcp.list_tools()

        # Peeked purely to size the compaction threshold below — the real,
        # possibly-different provider used per LLM call is (re-)resolved
        # fresh inside the loop each iteration (health can change mid-run).
        # Best-effort: an unhealthy/misconfigured provider here just falls
        # back to the global default rather than failing the turn early;
        # the loop's own resolution below is what actually surfaces that error.
        context_length_hint: int | None = None
        try:
            peek_provider = await self.router.get_provider(provider_override)
            context_length_hint = (peek_provider.config.config or {}).get("context_length")
        except NoHealthyProvider:
            pass

        messages = await self.context.build_messages(
            session_id, user_message, active_tools, persona_override,
            images=images, max_context_tokens_override=context_length_hint,
        )
        run_usage = {"input_tokens": 0, "output_tokens": 0}

        # Persisted incrementally (not just once at the very end, which is
        # all this used to do) so a window that isn't actively showing this
        # session's live deltas — because the user switched away mid-turn,
        # or the process was interrupted — can still resume a reasonably-
        # current view via `/history` instead of reverting all the way back
        # to the last fully-completed turn. `save_messages` already skips
        # any message carrying a DB `id` (prior, already-persisted history)
        # or a `system` role, so calling it repeatedly with only the
        # not-yet-saved tail is safe and doesn't re-insert anything twice —
        # *except* that a cancellation landing mid-write (this loop's own
        # `except asyncio.CancelledError` also calls this, to persist
        # whatever it can before giving up) could otherwise interrupt
        # `save_messages` after some rows are already inserted but before
        # `last_saved` advances, and a retry would then re-send — and
        # duplicate — that same slice. Advancing `last_saved` *before*
        # awaiting the write closes that: a slice is "claimed" the moment
        # we commit to sending it, so any retry only ever sees what's
        # genuinely new since then. The trade-off (an extremely
        # narrowly-timed cancel could skip persisting a message rather than
        # duplicate it) is the correct one — a missing row is far less
        # visibly wrong than a repeated one.
        last_saved = 0

        async def save_new_messages() -> None:
            nonlocal last_saved
            if len(messages) > last_saved:
                to_save = messages[last_saved:]
                last_saved = len(messages)
                await self.context.save_messages(session_id, to_save)

        await save_new_messages()  # the user's own message, right away

        done = False
        try:
            while not done:
                try:
                    provider = await self.router.get_provider(provider_override)
                except NoHealthyProvider:
                    await event_callback(SSEEvent(
                        type="error",
                        error_message="No healthy providers available",
                        session_id=session_id,
                        is_last=True,
                    ))
                    await event_callback(SSEEvent(
                        type="session_status",
                        session_id=session_id,
                        is_last=True,
                    ))
                    return

                # Progressive budget check
                in_budget_check = False
                tools_for_turn = _tools_to_openai_format(active_tools)
                if tool_call_count > 0 and tool_call_count % 20 == 0:
                    messages.append({"role": "system", "content": BUDGET_SYSTEM_MESSAGE})
                    in_budget_check = True
                    tools_for_turn = list(tools_for_turn) + [BUDGET_TOOL]

                # Stream LLM response
                # Accumulated as a list and joined once below rather than
                # `full_content +=` on every delta — CPython's in-place
                # string-append optimization isn't guaranteed once a
                # variable is read/held across `await` points in a loop
                # like this one, so this avoids a real (if usually small)
                # O(n^2) risk for very long streamed responses.
                content_chunks: list[str] = []
                turn_tool_calls: list[ToolCall] = []
                finish_reason: str | None = None
                turn_usage: dict[str, int] | None = None

                provider_config = provider.config.config or {}
                provider_msgs = _dicts_to_messages(messages)
                async for delta in provider.chat_completion(  # type: ignore[arg-type]
                    provider_msgs,
                    tools_for_turn,
                    model=model_override,
                    temperature=provider_config.get("temperature"),
                    top_p=provider_config.get("top_p"),
                ):
                    if delta.content:
                        content_chunks.append(delta.content)
                        await event_callback(SSEEvent(
                            type="llm_delta",
                            content=delta.content,
                            session_id=session_id,
                        ))
                    if delta.reasoning:
                        await event_callback(SSEEvent(
                            type="reasoning_delta",
                            content=delta.reasoning,
                            session_id=session_id,
                        ))
                    if delta.tool_calls:
                        turn_tool_calls.extend(delta.tool_calls)
                    if delta.usage:
                        turn_usage = delta.usage
                    if delta.finish_reason:
                        if delta.finish_reason == "error":
                            done = True
                        else:
                            finish_reason = delta.finish_reason

                if turn_usage:
                    run_usage["input_tokens"] += turn_usage.get("input_tokens", 0)
                    run_usage["output_tokens"] += turn_usage.get("output_tokens", 0)
                    await self.stats.record_usage(
                        session_id,
                        turn_usage.get("input_tokens", 0),
                        turn_usage.get("output_tokens", 0),
                        provider.provider_id,
                        provider.resolve_model(model_override),
                    )

                if done:
                    break

                # Budget check handling
                if in_budget_check:
                    has_budget = any(
                        tc.function.get("name") == "request_more_steps"
                        for tc in turn_tool_calls
                    )
                    has_other = any(
                        tc.function.get("name") != "request_more_steps"
                        for tc in turn_tool_calls
                    )
                    messages.pop()

                    if has_other and not has_budget:
                        err = (
                            "You have reached the step limit. "
                            "Call request_more_steps to continue or provide a final response."
                        )
                        await event_callback(SSEEvent(
                            type="tool_finish",
                            tool_name="__budget__",
                            tool_result=err,
                            session_id=session_id,
                        ))
                        # System role: a "tool" message here would be orphaned
                        # (no assistant tool_calls precede it) and OpenAI-strict
                        # endpoints reject that.
                        messages.append({"role": "system", "content": err})
                        continue

                    if has_budget:
                        turn_tool_calls = [
                            tc for tc in turn_tool_calls
                            if tc.function.get("name") != "request_more_steps"
                        ]
                        if not turn_tool_calls:
                            if finish_reason in ("stop", "end_turn"):
                                done = True
                            continue
                    else:
                        if finish_reason in ("stop", "end_turn"):
                            done = True
                        continue

                # Add assistant message to context
                assistant_msg: dict[str, Any] = {
                    "role": "assistant",
                    "content": "".join(content_chunks),
                }
                if turn_tool_calls:
                    assistant_msg["tool_calls"] = [
                        {
                            "id": tc.id,
                            "type": "function",
                            "function": tc.function,
                        }
                        for tc in turn_tool_calls
                    ]
                messages.append(assistant_msg)
                await save_new_messages()

                if not turn_tool_calls:
                    if finish_reason in ("stop", "end_turn"):
                        done = True
                        await event_callback(SSEEvent(
                            type="llm_stop",
                            session_id=session_id,
                            usage=dict(run_usage),
                        ))
                    break

                # Process every tool call from this turn concurrently — a
                # model that emits several at once (e.g. a RAG search + a web
                # search) should actually run them at the same time, not one
                # after another. `asyncio.gather` (not `TaskGroup`) is used
                # deliberately: `_run_one_tool_call` never raises (mirrors
                # `execute_tool`'s own exception-free, error-as-result
                # contract), so one call misbehaving can't cancel its
                # siblings. `gather` also preserves input order in its
                # results regardless of completion order, so replies below
                # are appended in the same order the model made the calls.
                tool_semaphore = asyncio.Semaphore(self.max_concurrent_tool_calls)
                call_results = await asyncio.gather(*[
                    self._run_one_tool_call(
                        session_id, tc, event_callback, tool_semaphore, allowed_dirs
                    )
                    for tc in turn_tool_calls
                ])

                # Loop detection is stateful and order-sensitive (a sliding
                # window over the last 3 calls) — run it as one deterministic
                # pass over the now-ordered results, not from inside each
                # concurrent coroutine, so scheduling order can't affect which
                # call trips it. Because every call already executed by this
                # point, "stopping" here means discarding the result of any
                # call after the tripping one and replacing it with a skipped
                # marker, rather than pre-empting execution — an accepted
                # trade-off for running tool calls concurrently.
                loop_stopped = False
                for tc, tool_args, result_text in call_results:
                    if loop_stopped:
                        result_text = "[Skipped: run stopped by loop detection]"
                    else:
                        tool_name = tc.function.get("name", "")
                        if self._check_repetition(session_id, tool_name, tool_args):
                            result_text = (
                                f"[Loop detected: '{tool_name}' was called identically "
                                f"3 times. Stopping.]"
                            )
                            await event_callback(SSEEvent(
                                type="error",
                                content=result_text,
                                session_id=session_id,
                                is_last=True,
                            ))
                            done = True
                            loop_stopped = True

                    # Every tool_call in the assistant message needs a matching
                    # tool reply (with its id), even on the loop-detected exit.
                    messages.append({
                        "role": "tool",
                        "content": result_text,
                        "tool_call_id": tc.id,
                    })
                    tool_call_count += 1

                await save_new_messages()

                if done:
                    break

            await save_new_messages()

            if not session.get("name"):
                title = _derive_title(user_message)
                if title:
                    await self.db.execute(
                        "UPDATE sessions SET name = :name WHERE id = :id",
                        {"id": session_id, "name": title},
                    )
                    await event_callback(SSEEvent(
                        type="session_title",
                        content=title,
                        session_id=session_id,
                    ))

        except asyncio.CancelledError:
            # Best-effort: persist whatever accumulated before the
            # cancellation, so switching away (or a cancel from elsewhere)
            # doesn't discard an otherwise-complete tool round just because
            # the run didn't reach its normal end-of-turn save point.
            try:
                await save_new_messages()
            except Exception as e:
                logger.warning("save_new_messages on cancel failed for %s: %s", session_id, e)
            await event_callback(SSEEvent(
                type="session_status",
                session_id=session_id,
                content="Cancelled",
                is_last=True,
            ))
        finally:
            self._loop_history.pop(session_id, None)
            # Nothing to pop here by session_id — `_hitl_events` is keyed by
            # action_id, and each entry already cleans itself up in the
            # try/finally around its own `approval_event.wait()` above.
            await event_callback(SSEEvent(
                type="session_status",
                session_id=session_id,
                is_last=True,
            ))
            # Fired from `finally`, not from the `llm_stop` branch above:
            # `llm_stop` only fires on the clean no-tool-calls stop path and
            # misses error/cancel/loop-detection exits — exactly the
            # tool-heavy runs most likely to need compaction. `finally` runs
            # on every path. This is fire-and-forget and must never block
            # or fail this turn's response.
            self._schedule_compaction(session_id, event_callback, context_length_hint)

    def _schedule_compaction(
        self,
        session_id: str,
        event_callback: Callable[[SSEEvent], Awaitable[None]],
        context_length_hint: int | None,
    ) -> None:
        if self.summarizer is None or not self.summarizer_config.enabled:
            return
        context_length = context_length_hint or self.token_management_config.max_context_tokens
        task = asyncio.create_task(
            self._run_compaction_and_notify(session_id, event_callback, context_length)
        )
        self._compaction_tasks.add(task)
        task.add_done_callback(self._compaction_tasks.discard)

    async def _run_compaction_and_notify(
        self,
        session_id: str,
        event_callback: Callable[[SSEEvent], Awaitable[None]],
        context_length: int,
    ) -> None:
        try:
            result = await run_compaction(
                session_id,
                self.db,
                self.summarizer,
                self.token_management_config,
                self.summarizer_config,
                context_length,
            )
        except Exception:
            logger.exception("compaction: background pass crashed for %s", session_id)
            return
        if result is None:
            return
        try:
            await event_callback(SSEEvent(
                type="compaction",
                session_id=session_id,
                content=(
                    f"Compacted {result.messages_compacted} messages "
                    f"({result.tokens_before} -> {result.tokens_after} tokens)"
                ),
            ))
        except Exception:
            # The turn's SSE stream may already be closed by the time a
            # background compaction pass finishes (the client disconnected,
            # or drained the queue and returned) — this notification is
            # best-effort only, never worth failing the pass over.
            logger.debug("compaction: could not emit event for %s (stream closed?)", session_id)

    async def shutdown(self) -> None:
        """Cancels any in-flight background compaction passes. Called from
        the daemon's lifespan shutdown so a pass doesn't race the database
        connection closing right after it.
        """
        for task in list(self._compaction_tasks):
            task.cancel()
        for task in list(self._compaction_tasks):
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass

    async def _run_one_tool_call(
        self,
        session_id: str,
        tc: ToolCall,
        event_callback: Callable[[SSEEvent], Awaitable[None]],
        semaphore: asyncio.Semaphore,
        allowed_dirs: list[str],
    ) -> tuple[ToolCall, dict[str, Any], str]:
        """The HITL-check -> possible-approval-wait -> execute sequence for
        one tool call, independent of any other call in the same turn — the
        caller runs N of these concurrently via `asyncio.gather`. Returns
        `(tc, tool_args, result_text)`; the caller re-parses nothing, since
        `tool_args` is threaded back out for loop detection. Never raises, so
        one call's failure can't cancel its siblings via `gather`; the
        `semaphore` only gates the actual `execute_tool` call, not the
        (potentially long) approval wait, so a pending approval doesn't tie
        up a concurrency slot doing nothing."""
        tool_name = tc.function.get("name", "")
        raw_args = tc.function.get("arguments", "{}")
        try:
            tool_args = (
                json.loads(raw_args)
                if isinstance(raw_args, str)
                else raw_args
            )
        except json.JSONDecodeError:
            tool_args = {}

        await event_callback(SSEEvent(
            type="tool_start",
            tool_name=tool_name,
            tool_args=tool_args,
            session_id=session_id,
        ))

        hitl_result = await self.hitl.check_tool_call(
            session_id, tool_name, tool_args
        )
        # Mandatory directory-sandboxing gate: a call this policy would
        # otherwise let through outright is force-escalated to needs_approval
        # if it touches a path outside this session's allowed directories.
        # This can only *upgrade* proceed/always_allow — it never downgrades
        # an existing `rejected` decision (an auto_reject_patterns match
        # still wins outright). An `always_allow` DB rule means "stop asking
        # about this tool," not "let it touch arbitrary directories."
        if hitl_result.action in ("proceed", "always_allow") and not sandbox.check_containment(
            tool_args, allowed_dirs
        ):
            hitl_result = self.hitl.force_approval(session_id, tool_name, tool_args)

        should_execute = False
        result_text = ""

        if hitl_result.action == "rejected":
            result_text = (
                f"[Tool call '{tool_name}' rejected by safety policy: "
                f"{hitl_result.reason or 'No reason'}]"
            )
            await event_callback(SSEEvent(
                type="tool_finish",
                tool_name=tool_name,
                tool_result=result_text,
                session_id=session_id,
            ))

        elif hitl_result.action == "needs_approval":
            await event_callback(SSEEvent(
                type="hitl_pause",
                tool_name=tool_name,
                tool_args=tool_args,
                session_id=session_id,
                action_id=hitl_result.pending_action_id,
                content=f"Action {hitl_result.pending_action_id} needs approval",
            ))

            approval_event = asyncio.Event()
            self._hitl_events[hitl_result.pending_action_id] = approval_event
            try:
                await approval_event.wait()
            finally:
                self._hitl_events.pop(hitl_result.pending_action_id, None)

            decision = (
                self.hitl.pop_decision(hitl_result.pending_action_id)
                if hitl_result.pending_action_id
                else None
            )
            await event_callback(SSEEvent(
                type="hitl_resolved",
                tool_name=tool_name,
                content=decision or "unresolved",
                session_id=session_id,
            ))

            if decision in ("allow", "always_allow"):
                should_execute = True
            else:
                result_text = f"[Tool call '{tool_name}' was not approved]"
                await event_callback(SSEEvent(
                    type="tool_finish",
                    tool_name=tool_name,
                    tool_result=result_text,
                    session_id=session_id,
                ))

        else:
            should_execute = True

        if should_execute:
            async with semaphore:
                tool_result = await self.mcp.execute_tool(tool_name, tool_args)
            if tool_result.is_error:
                result_text = f"[Tool '{tool_name}' error: {tool_result.content}]"
            else:
                result_text = tool_result.content
            await event_callback(SSEEvent(
                type="tool_finish",
                tool_name=tool_name,
                tool_result=result_text,
                session_id=session_id,
                duration_ms=tool_result.duration_ms,
            ))

        return tc, tool_args, result_text

    async def cancel(self, session_id: str) -> None:
        task = self._tasks.get(session_id)
        if task and not task.done():
            task.cancel()

        # `_hitl_events` is keyed by action_id, so cancelling a session must
        # wake every one of its pending approval waits, not just one.
        for pending in self.hitl.get_pending_approvals(session_id):
            event = self._hitl_events.get(pending.action_id)
            if event:
                event.set()

        await self.db.execute(
            "UPDATE sessions SET status = 'idle' WHERE id = :id",
            {"id": session_id},
        )

    def _check_repetition(
        self,
        session_id: str,
        tool_name: str,
        tool_args: dict[str, Any],
    ) -> bool:
        h = hashlib.sha256(
            f"{tool_name}:{json.dumps(tool_args, sort_keys=True)}".encode()
        ).hexdigest()
        history = self._loop_history.setdefault(session_id, [])
        history.append(h)
        if len(history) > 3:
            history.pop(0)
        return len(history) == 3 and all(x == history[0] for x in history)
