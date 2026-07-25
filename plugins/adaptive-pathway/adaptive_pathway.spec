# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the Adaptive Pathway HTTP sidecar. Run via
# `plugins/build.py`, not directly — that script also `pip install`s this
# plugin's dependencies first so PyInstaller's import analysis can see them.
#
# Freezes `adaptive_pathway.integrations.sidecar.__main__:main` (the same
# entry point named in pyproject.toml's `adaptive-pathway-sidecar` console
# script) into a single onefile executable. `config/defaults.yaml` is bundled
# as a data file at the same relative path the package expects at runtime
# (`engine.py` resolves it via `Path(__file__).parent / "config" / "defaults.yaml"`,
# which still works inside a PyInstaller onefile bundle's extraction dir).

a = Analysis(
    ["src/adaptive_pathway/integrations/sidecar/__main__.py"],
    pathex=["src"],
    binaries=[],
    datas=[
        ("src/adaptive_pathway/config/defaults.yaml", "adaptive_pathway/config"),
    ],
    # uvicorn/fastapi resolve some of their own submodules dynamically at
    # runtime (loop/protocol implementation selection) — PyInstaller's static
    # import analysis can miss these. If the frozen exe fails at startup with
    # a ModuleNotFoundError, add the missing module here.
    hiddenimports=[
        "uvicorn.logging",
        "uvicorn.loops",
        "uvicorn.loops.auto",
        "uvicorn.protocols",
        "uvicorn.protocols.http",
        "uvicorn.protocols.http.auto",
        "uvicorn.protocols.websockets",
        "uvicorn.protocols.websockets.auto",
        "uvicorn.lifespan",
        "uvicorn.lifespan.on",
        # Same gap as adaptive_pathway_mcp.spec: SQLAlchemy resolves the
        # "aiosqlite" DBAPI driver dynamically from the `sqlite+aiosqlite://`
        # URL (storage/database.py), not via a plain top-level import, so
        # PyInstaller's static analysis misses it without this.
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
    name="adaptive-pathway-sidecar",
    console=True,
    onefile=True,
)
