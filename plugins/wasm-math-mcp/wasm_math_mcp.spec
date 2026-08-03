# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the sandboxed-Python-execution stdio MCP server. Run via
# `plugins/build.py`, not directly — that script `pip install`s this plugin's
# dependencies first so PyInstaller's import analysis can see them.

from PyInstaller.utils.hooks import copy_metadata

a = Analysis(
    ["wasm_math_mcp.py"],
    pathex=["."],
    binaries=[],
    datas=[]
    # The `mcp` SDK resolves its own package metadata at import time in some
    # versions — same PackageNotFoundError failure mode already fixed for
    # `fastmcp` in replacement_mcp.spec and `mcp` in adaptive_pathway_mcp.spec.
    + copy_metadata("mcp"),
    hiddenimports=[
        "mcp.server.fastmcp",
        # The sandbox's exposed module set was rewritten to a stdlib +
        # NetworkX-only "Zero-Heavy-Dependency Stack" (see wasm_math_mcp.py's
        # SAFE_GLOBALS) — numpy/pandas/scipy are no longer imported anywhere
        # and must NOT be re-added here: they'd force PyInstaller to resolve
        # packages this plugin no longer declares as dependencies (see
        # pyproject.toml), which fails outright in an isolated build venv
        # that never installed them, and silently reintroduces the multi-
        # hundred-MB bloat this rewrite was for otherwise.
        "networkx",
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
    name="wasm-math-mcp",
    console=True,
    onefile=True,
)
