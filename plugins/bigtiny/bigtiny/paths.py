"""BigTiny's own persistent-data root: its SQLite database, the directory-
sandbox's always-allowed cache dir (`agent/sandbox.py`'s `CACHE_DIR`), and the
recipes directory (`recipes/engine.py`) all live under the same root so they
move together as a single concept.

Overridable via `BIGTINY_DATA_DIR` — Kitty sets this when spawning the daemon
to consolidate everything under `%APPDATA%\\Kitty\\bigtiny\\` rather than
leaving it at the standalone default below, which remains what a bare
`python -m bigtiny` (no Kitty, no env var) uses for local dev/testing.
"""

from __future__ import annotations

import os
from pathlib import Path


def data_dir() -> str:
    override = os.environ.get("BIGTINY_DATA_DIR")
    base = override if override else "~/.bigtiny"
    return str(Path(base).expanduser())
