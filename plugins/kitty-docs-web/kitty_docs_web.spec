# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the kitty-docs-web stdio MCP server. Run via
# `plugins/build.py`, not directly — that script `pip install`s this
# plugin's dependencies first so PyInstaller's import analysis can see them.

from PyInstaller.utils.hooks import copy_metadata

a = Analysis(
    ["kitty_docs_web.py"],
    pathex=["."],
    binaries=[],
    datas=[]
    # fastmcp resolves its own `__version__` from installed package metadata
    # at import time — without this the frozen exe dies during `import
    # fastmcp` with PackageNotFoundError before serving a single request
    # (same fix as replacement-mcp/adaptive-pathway).
    + copy_metadata("fastmcp"),
    hiddenimports=[
        "fastmcp",
        # `ddgs` is imported lazily inside `_ddg_query` (called from
        # `lean_web_search`), so a missed import wouldn't surface until the
        # first search rather than at startup.
        "ddgs",
        # PyMuPDF's compiled extension module; PyInstaller's static analysis
        # can miss it since it's imported as `fitz`, not `pymupdf`.
        "fitz",
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
    name="kitty-docs-web",
    console=True,
    onefile=True,
)
