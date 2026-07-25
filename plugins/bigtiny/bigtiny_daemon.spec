# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec for the BigTiny daemon. Run via Kitty's `plugins/build.py`,
# not directly — that script `pip install`s this package first so
# PyInstaller's import analysis can see its dependencies.
#
# Freezes `bigtiny.__main__:main` (the same entry point named in
# pyproject.toml's `bigtiny-daemon` console script) into a single onefile
# executable. uvicorn resolves the app factory and event-loop factory by
# dotted string (`bigtiny.server.app:create_app` /
# `bigtiny.server.app:loop_factory`) at startup, so the whole `bigtiny`
# package must be importable from the frozen bundle, not just the modules
# PyInstaller's static analysis reaches from `__main__.py` alone.

from PyInstaller.utils.hooks import collect_submodules, copy_metadata

a = Analysis(
    ["bigtiny/__main__.py"],
    pathex=["."],
    binaries=[],
    # tiktoken loads its BPE rank files from its own package metadata / a
    # bundled data file at runtime — bundle its dist-info so that resolution
    # doesn't fail inside the frozen exe's extraction dir.
    datas=copy_metadata("tiktoken"),
    hiddenimports=(
        collect_submodules("bigtiny")
        + [
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
        ]
    ),
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
    name="bigtiny-daemon",
    console=True,
    onefile=True,
)
