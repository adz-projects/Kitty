# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the replacement-mcp stdio MCP server. Run via
# `plugins/build.py`, not directly — that script `pip install`s this
# plugin's dependencies first so PyInstaller's import analysis can see them.
#
# `tool_prompts.yaml` is bundled as a data file next to the frozen exe's
# extraction root (`lean_mcp.py` resolves it via
# `Path(__file__).resolve().parent / "tool_prompts.yaml"`, which still works
# inside a PyInstaller onefile bundle's extraction dir).

a = Analysis(
    ["lean_mcp.py"],
    pathex=["."],
    binaries=[],
    datas=[
        ("tool_prompts.yaml", "."),
    ],
    # fastmcp/trafilatura/duckduckgo-search occasionally need their vendored
    # submodules named explicitly for PyInstaller's static analysis. If the
    # frozen exe fails at startup with a ModuleNotFoundError, add it here.
    hiddenimports=[
        "fastmcp",
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
    name="replacement-mcp",
    console=True,
    onefile=True,
)
