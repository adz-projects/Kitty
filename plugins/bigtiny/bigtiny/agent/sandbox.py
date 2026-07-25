"""Directory-based sandboxing for tool calls — the authoritative gate.

Kitty's own client-side pre-filter (`src/stores/chat/approvalUtils.ts`,
`pathWithinDir`/`decideChatApproval`) does an analogous, purely lexical
containment check, but it only ever ran in chat mode and only ever guards
the approval *prompt UI* — a determined or future non-Kitty client hitting
BigTiny's REST API directly would bypass it entirely. This module is the
real security boundary: it runs inside the agent loop itself, for every
tool call, in both chat and agentic mode.

Every session has an effective allowed-directory set derived from its
metadata (`chat_dir`, `cwd`, `mode` — see `bigtiny/server/routes/chat.py`):
  - chat mode:    {chat_dir, cache_dir}
  - agentic mode: {chat_dir, cache_dir, cwd} — `cwd` starts equal to
    `chat_dir` and may diverge via "Set as working directory"; both remain
    allowed even after it diverges.

Containment is purely lexical (path-segment manipulation, no
`Path.resolve()`, no filesystem access) — deliberately mirroring
`pathWithinDir`'s semantics rather than reaching for a "more correct"
filesystem-resolving check, since resolving a not-yet-existing path behaves
inconsistently and Kitty's own client-side pre-filter (kept as a UX nicety,
not the security boundary) needs to agree with this module on what counts
as in-bounds.

Shell/terminal-style tool calls don't have a single structured path argument
the way a file tool does, so their coverage here is intentionally
best-effort (see `extract_shell_paths`) — a command with no literal path, or
one built from a variable at runtime, won't be caught. This is an accepted
trade-off, not a gap to silently paper over: anything this module can't
positively confirm as in-bounds is treated as in-bounds (fail-open) rather
than blocking legitimate shell use with no realistic sandboxing story.
"""

from __future__ import annotations

import re
from typing import Any

from bigtiny import paths

# BigTiny's own app-data directory (DB, logs, recipes — same root
# `storage.Database`'s default `db_path` and `recipes/engine.py`'s default
# `recipes_dir` resolve via `bigtiny.paths.data_dir()`) — always allowed
# regardless of mode, so internal housekeeping tools never trip the sandbox.
CACHE_DIR = paths.data_dir()


def _norm(path: str) -> str:
    p = path.replace("\\", "/")
    while p.endswith("/") and len(p) > 1:
        p = p[:-1]
    return p.lower()


def path_within_any(bases: list[str], target: str) -> bool:
    """True if `target` lexically resolves inside at least one of `bases`.

    Direct port of Kitty's `pathWithinDir` (`approvalUtils.ts`), generalized
    to multiple bases: an absolute target keeps its own drive/root; a
    relative one resolves against whichever base is being checked; `.`/`..`
    segments collapse via a stack; comparison is case-insensitive (Windows).
    """
    t = target.replace("\\", "/")
    is_absolute = bool(re.match(r"^[a-zA-Z]:/", t)) or t.startswith("/")
    for base in bases:
        if not base:
            continue
        b = _norm(base)
        candidate = t if is_absolute else f"{b}/{t}"
        has_drive = bool(re.match(r"^[a-zA-Z]:", candidate))
        drive = candidate[:2] if has_drive else ""
        rest = candidate[2:] if has_drive else candidate
        stack: list[str] = []
        for seg in rest.split("/"):
            if seg in ("", "."):
                continue
            if seg == "..":
                if stack:
                    stack.pop()
                continue
            stack.append(seg)
        resolved = _norm(f"{drive}/{'/'.join(stack)}")
        if resolved == b or resolved.startswith(f"{b}/"):
            return True
    return False


# Common structured-path argument names across MCP tool schemas (read/write/
# edit/list/etc.) — mirrors `decideChatApproval`'s own extraction
# (`input.path ?? input.file_path ?? input.paths?.[0]`).
_PATH_KEYS = ("path", "file_path", "directory", "dir")


def extract_candidate_paths(args: dict[str, Any]) -> list[str]:
    """Structured-argument paths a tool call is asking to touch — empty if
    the tool has no path-like argument at all (nothing to check, trivially
    in-bounds)."""
    found: list[str] = []
    for key in _PATH_KEYS:
        value = args.get(key)
        if isinstance(value, str) and value:
            found.append(value)
    paths = args.get("paths")
    if isinstance(paths, list) and paths and isinstance(paths[0], str):
        found.append(paths[0])
    return found


# Best-effort extraction of literal filesystem paths from a shell command
# string. NOT a security boundary by itself — see the module docstring —
# it only feeds `check_containment`, which forces a human approval on a
# positive out-of-bounds match; it never *allows* something a plain
# containment check wouldn't have caught. Matches a quoted or bare Windows
# drive-letter path, plus a bare POSIX-style rooted/relative path (in case
# BigTiny ever runs under WSL or mixed tooling).
_SHELL_PATH_RE = re.compile(
    r'"([A-Za-z]:[^"]+)"'
    r"|'([A-Za-z]:[^']+)'"
    r'|([A-Za-z]:[\\/][^\s"\']+)'
    r"|(\.{0,2}/[^\s\"']+)"
)


def extract_shell_paths(command: str) -> list[str]:
    """Best-effort: literal paths embedded in a shell command string."""
    found: list[str] = []
    for match in _SHELL_PATH_RE.finditer(command):
        found.append(next(g for g in match.groups() if g))
    return found


# Shell/terminal-style tools rarely have a single structured "path" argument
# the way a file tool does — their payload is usually under one of these
# string keys instead.
_SHELL_ARG_KEYS = ("command", "cmd", "script")


def check_containment(args: dict[str, Any], allowed_dirs: list[str]) -> bool:
    """True if every path this tool call touches — structured arguments plus
    a best-effort scan of any shell-command-shaped string argument —
    resolves inside at least one of `allowed_dirs`. A call with no
    path-like argument at all (e.g. a pure-math tool) is trivially
    in-bounds; there's nothing to check."""
    candidates = extract_candidate_paths(args)
    for key in _SHELL_ARG_KEYS:
        value = args.get(key)
        if isinstance(value, str) and value:
            candidates.extend(extract_shell_paths(value))
    return all(path_within_any(allowed_dirs, p) for p in candidates)


def allowed_dirs_for_session(metadata: dict[str, Any], cache_dir: str) -> list[str]:
    """The effective allowed-directory set for a session, from its stored
    metadata (`chat_dir`/`cwd`/`mode` — see `bigtiny/server/routes/chat.py`).
    Agentic mode additionally allows the session's current `cwd`, which may
    have diverged from `chat_dir` via "Set as working directory"; chat mode
    never diverges the two (no UI path to change cwd), so listing both is
    harmless there too, not just for agentic sessions."""
    dirs = [metadata.get("chat_dir"), cache_dir]
    if metadata.get("mode") == "agentic":
        dirs.append(metadata.get("cwd"))
    return [d for d in dirs if d]
