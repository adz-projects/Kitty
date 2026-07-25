# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the Adaptive Pathway MCP tools (`decide`/`record_outcome`/
# etc.) — registered as a stdio MCP server with BigTiny (see
# `bigtiny::mcp::ensure_builtin_servers` on the Kitty side). Run via
# `plugins/build.py`, not directly — that script `pip install`s this plugin's
# `mcp` extra first so PyInstaller's import analysis can see it.
#
# Freezes `adaptive_pathway.mcp_server:main` (the same entry point named in
# pyproject.toml's `adaptive-pathway-mcp` console script) into a single
# onefile executable.

from PyInstaller.utils.hooks import copy_metadata

a = Analysis(
    ["src/adaptive_pathway/mcp_server.py"],
    pathex=["src"],
    binaries=[],
    datas=[
        ("src/adaptive_pathway/config/defaults.yaml", "adaptive_pathway/config"),
    ]
    # The `mcp` SDK resolves its own package metadata at import time in some
    # versions (mirrors the same PackageNotFoundError failure mode fixed for
    # `fastmcp` in replacement_mcp.spec) — bundle it defensively.
    + copy_metadata("mcp"),
    hiddenimports=[
        "mcp.server.fastmcp",
        # SQLAlchemy's async engine resolves the "aiosqlite" DBAPI driver by
        # dynamically importing it from the `sqlite+aiosqlite://` URL scheme
        # (storage/database.py's create_async_engine call) rather than a
        # plain top-level `import aiosqlite` — invisible to PyInstaller's
        # static bytecode analysis, so it must be listed explicitly or the
        # frozen exe fails every DB-touching tool call at runtime with
        # "No module named 'aiosqlite'" (confirmed via a live probe of the
        # frozen exe's `decide` tool).
        "aiosqlite",
    ],
    hookspath=[],
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
)
pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name="adaptive-pathway-mcp",
    console=True,
    onefile=True,
)
