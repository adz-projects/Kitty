#!/usr/bin/env python3
"""Freezes every internal plugin to a standalone Windows .exe via PyInstaller
and drops the result into src-tauri/binaries/ with Tauri's externalBin
target-triple naming convention, so `tauri build` bundles them automatically.

Usage:
    python plugins/build.py            # build every plugin
    python plugins/build.py <name>...  # build only the named plugin(s)

Requires PyInstaller (`pip install pyinstaller`) and each plugin's own
dependencies — this script installs those for you via `pip install -e .`
before freezing, so run it with the Python environment you want the freeze
to use (a dedicated venv is recommended, not your system Python).

Windows-only app, so the target triple is hardcoded to the one Kitty ships
(see docs/VERSIONS.md).
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

TARGET_TRIPLE = "x86_64-pc-windows-msvc"

PLUGINS_DIR = Path(__file__).resolve().parent
REPO_ROOT = PLUGINS_DIR.parent
BINARIES_DIR = REPO_ROOT / "src-tauri" / "binaries"

# name: (spec file, frozen exe name — must match pyproject.toml's console script
# name, since that's also what the Rust side's launch-command override expects).
PLUGINS = {
    "adaptive-pathway": ("adaptive_pathway.spec", "adaptive-pathway-sidecar"),
    "replacement-mcp": ("replacement_mcp.spec", "replacement-mcp"),
}


def run(cmd: list[str], cwd: Path) -> None:
    print(f"$ {' '.join(cmd)}  (in {cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def build_plugin(name: str) -> None:
    if name not in PLUGINS:
        raise SystemExit(f"unknown plugin: {name} (known: {', '.join(PLUGINS)})")
    spec_file, exe_name = PLUGINS[name]
    plugin_dir = PLUGINS_DIR / name
    if not (plugin_dir / spec_file).exists():
        raise SystemExit(f"{plugin_dir / spec_file} not found")

    print(f"\n=== {name} ===")
    # Install the plugin's own pinned dependencies into whatever Python
    # environment is running this script, so PyInstaller's import analysis
    # can see them. `-e` keeps this reusable during local dev without a
    # reinstall on every source edit.
    run([sys.executable, "-m", "pip", "install", "-e", "."], cwd=plugin_dir)
    run([sys.executable, "-m", "pip", "install", "pyinstaller"], cwd=plugin_dir)

    run(
        [
            sys.executable,
            "-m",
            "PyInstaller",
            spec_file,
            "--noconfirm",
            "--distpath",
            "dist",
            "--workpath",
            "build",
        ],
        cwd=plugin_dir,
    )

    built_exe = plugin_dir / "dist" / f"{exe_name}.exe"
    if not built_exe.exists():
        raise SystemExit(f"expected PyInstaller output at {built_exe}, but it's missing")

    BINARIES_DIR.mkdir(parents=True, exist_ok=True)
    dest = BINARIES_DIR / f"{exe_name}-{TARGET_TRIPLE}.exe"
    shutil.copy2(built_exe, dest)
    print(f"-> {dest}")


def main() -> None:
    names = sys.argv[1:] or list(PLUGINS)
    for name in names:
        build_plugin(name)
    print(f"\nDone. Frozen binaries are in {BINARIES_DIR}")


if __name__ == "__main__":
    main()
