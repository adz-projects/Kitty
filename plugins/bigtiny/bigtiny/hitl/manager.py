from __future__ import annotations

import json
import re
from datetime import datetime, timedelta
from typing import Any, Literal
from uuid import uuid4

from bigtiny.config import HITLConfig
from bigtiny.storage import Database

# An approval the client never answers — disconnect, force-quit without
# hitting /cancel — would otherwise leak its _pending/_session_pending/
# _decisions entries forever, since nothing else ever removes them. Swept
# on access (see `_sweep_stale`) rather than via a dedicated periodic task,
# so the cost is amortized against the handful of calls that already touch
# this state instead of adding new background-task overhead.
MAX_PENDING_AGE = timedelta(hours=1)


class PendingAction:
    def __init__(
        self,
        action_id: str,
        session_id: str,
        tool_name: str,
        tool_args: dict[str, Any],
    ):
        self.action_id = action_id
        self.session_id = session_id
        self.tool_name = tool_name
        self.tool_args = tool_args
        self.created_at = datetime.utcnow()

    def to_dict(self) -> dict[str, Any]:
        return {
            "action_id": self.action_id,
            "session_id": self.session_id,
            "tool_name": self.tool_name,
            "tool_args": self.tool_args,
            "created_at": self.created_at.isoformat(),
        }


class HITLDecision:
    def __init__(
        self,
        action: Literal["proceed", "needs_approval", "rejected", "always_allow"],
        reason: str | None = None,
        pending_action_id: str | None = None,
    ):
        self.action = action
        self.reason = reason
        self.pending_action_id = pending_action_id

    def to_dict(self) -> dict[str, Any]:
        return {
            "action": self.action,
            "reason": self.reason,
            "pending_action_id": self.pending_action_id,
        }


class HITLManager:
    def __init__(self, db: Database, config: HITLConfig):
        self.db = db
        self.config = config
        self._pending: dict[str, PendingAction] = {}
        self._session_pending: dict[str, list[str]] = {}
        # action_id -> (resolved decision, resolved_at). The timestamp is
        # only for `_sweep_stale` below — `pop_decision` still hands back
        # just the decision string to callers.
        self._decisions: dict[str, tuple[str, datetime]] = {}

    def _sweep_stale(self) -> None:
        cutoff = datetime.utcnow() - MAX_PENDING_AGE
        stale_pending = [aid for aid, p in self._pending.items() if p.created_at < cutoff]
        for aid in stale_pending:
            pending = self._pending.pop(aid, None)
            if not pending:
                continue
            session_list = self._session_pending.get(pending.session_id, [])
            if aid in session_list:
                session_list.remove(aid)
            if not session_list:
                self._session_pending.pop(pending.session_id, None)

        stale_decisions = [aid for aid, (_, ts) in self._decisions.items() if ts < cutoff]
        for aid in stale_decisions:
            self._decisions.pop(aid, None)

    async def check_tool_call(
        self,
        session_id: str,
        tool_name: str,
        args: dict[str, Any],
    ) -> HITLDecision:
        args_str = json.dumps(args, sort_keys=True)

        for pattern in self.config.auto_reject_patterns:
            if pattern in args_str or pattern in tool_name:
                await self._log_rule_match(session_id, tool_name, args, "reject")
                return HITLDecision(
                    action="rejected",
                    reason=f"Tool call matched auto-reject pattern: {pattern}",
                )

        for pattern in self.config.always_allow_patterns:
            if pattern in tool_name or (pattern in args_str):
                await self._log_rule_match(session_id, tool_name, args, "always_allow")
                return HITLDecision(action="always_allow")

        rule = await self._check_db_rules(tool_name, args_str)
        if rule:
            if rule["decision"] == "reject":
                return HITLDecision(
                    action="rejected",
                    reason=f"DB rule prevents this tool call: {rule.get('args_pattern', tool_name)}",
                )
            elif rule["decision"] in ("allow", "always_allow"):
                return HITLDecision(action="proceed")

        if self.config.default_policy == "auto_allow":
            return HITLDecision(action="proceed")
        elif self.config.default_policy == "auto_reject":
            return HITLDecision(
                action="rejected",
                reason="Default policy is auto-reject for unclassified tool calls",
            )

        return self._create_pending(session_id, tool_name, args, "requires human approval")

    def _create_pending(
        self,
        session_id: str,
        tool_name: str,
        args: dict[str, Any],
        reason: str,
    ) -> HITLDecision:
        self._sweep_stale()
        action_id = uuid4().hex
        pending = PendingAction(
            action_id=action_id,
            session_id=session_id,
            tool_name=tool_name,
            tool_args=args,
        )
        self._pending[action_id] = pending
        self._session_pending.setdefault(session_id, []).append(action_id)

        return HITLDecision(
            action="needs_approval",
            reason=f"Tool '{tool_name}' {reason}",
            pending_action_id=action_id,
        )

    def force_approval(
        self,
        session_id: str,
        tool_name: str,
        args: dict[str, Any],
        reason: str = "wants to touch a path outside this session's allowed directories",
    ) -> HITLDecision:
        """Mandatory escalation to `needs_approval`, bypassing every rule in
        `check_tool_call`'s own precedence chain — including a persisted
        `always_allow` DB rule for this tool name. Called from
        `bigtiny/agent/loop.py` only to *upgrade* an already-`proceed`/
        `always_allow` decision when `bigtiny.agent.sandbox.check_containment`
        finds an out-of-bounds path; it never downgrades an existing
        `rejected` decision (an `auto_reject_patterns` match still wins
        outright — see the call site). An `always_allow` rule means "stop
        asking about this tool," not "let it touch arbitrary directories,"
        so this override is intentional, not a bug."""
        return self._create_pending(session_id, tool_name, args, reason)

    async def _check_db_rules(
        self,
        tool_name: str,
        args_str: str,
    ) -> dict[str, Any] | None:
        rows = await self.db.fetch_all(
            "SELECT * FROM hitl_rules WHERE tool_name = :name",
            {"name": tool_name},
        )
        for row in rows:
            if row.get("args_pattern"):
                try:
                    if re.search(row["args_pattern"], args_str):
                        return row
                except re.error:
                    if row["args_pattern"] in args_str:
                        return row
            else:
                return row
        return None

    async def _log_rule_match(
        self,
        session_id: str,
        tool_name: str,
        args: dict[str, Any],
        decision: str,
    ) -> None:
        pass

    async def record_decision(
        self,
        action_id: str,
        decision: Literal["allow", "always_allow", "reject"],
    ) -> HITLDecision:
        pending = self._pending.pop(action_id, None)
        if not pending:
            return HITLDecision(
                action="rejected",
                reason=f"No pending action found: {action_id}",
            )

        session_list = self._session_pending.get(pending.session_id, [])
        if action_id in session_list:
            session_list.remove(action_id)
        if not session_list:
            self._session_pending.pop(pending.session_id, None)

        self._decisions[action_id] = (decision, datetime.utcnow())

        if decision == "reject":
            return HITLDecision(
                action="rejected",
                reason=f"User rejected tool call '{pending.tool_name}'",
            )

        if decision == "always_allow":
            await self.db.execute(
                """INSERT INTO hitl_rules (tool_name, args_pattern, decision)
                   VALUES (:name, :pattern, :decision)""",
                {
                    "name": pending.tool_name,
                    "pattern": None,
                    "decision": "always_allow",
                },
            )
            return HITLDecision(action="always_allow")

        return HITLDecision(action="proceed")

    def pop_decision(self, action_id: str) -> str | None:
        """Consume the resolved decision for an action, if any."""
        entry = self._decisions.pop(action_id, None)
        return entry[0] if entry else None

    def get_pending_approvals(self, session_id: str) -> list[PendingAction]:
        action_ids = self._session_pending.get(session_id, [])
        return [self._pending[aid] for aid in action_ids if aid in self._pending]

    async def cancel_pending(self, session_id: str) -> None:
        action_ids = self._session_pending.pop(session_id, [])
        for aid in action_ids:
            self._pending.pop(aid, None)
