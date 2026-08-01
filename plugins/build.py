#!/usr/bin/env python3
"""Freezes every internal plugin (and the BigTiny daemon) to a standalone
Windows .exe via PyInstaller and drops the result into src-tauri/binaries/
with Tauri's externalBin target-triple naming convention, so `tauri build`
bundles them automatically.

Usage:
    python plugins/build.py            # build every target
    python plugins/build.py <name>...  # build only the named target(s)

Requires PyInstaller (`pip install pyinstaller`) and each target's own
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

# name -> { dir, spec, exe, extras, kind }. `exe` must match pyproject.toml's
# console script name (Python) or Cargo.toml's [[bin]] name (Rust), since
# that's also what the Rust side's launch-command override expects. `extras`
# are optional-dependency-group names installed alongside the base package
# (`pip install -e ".[extra1,extra2]"`) — ignored for `kind: "rust"`. `kind`
# defaults to `"python"` (see `build_plugin`) so every pre-existing entry is
# unchanged; only `kitty-tools` sets it to `"rust"`.
#
# `replacement-mcp`, `brave-mcp-search`, and `visualizations` are retired —
# their tools are all hosted inside `kitty-tools` now (the Rust
# consolidation) — and deliberately absent from this dict, so
# `python plugins/build.py` (no args) no longer builds or bundles them. Their
# source files stay in-tree, unbuilt, as the oracle for re-verifying kitty-tools
# against if a behavioral gap ever surfaces (see docs/PLUGINS.md).
PLUGINS: dict[str, dict[str, object]] = {
    "adaptive-pathway": {
        "dir": PLUGINS_DIR / "adaptive-pathway",
        "spec": "adaptive_pathway.spec",
        "exe": "adaptive-pathway-sidecar",
        "extras": ["sidecar"],
    },
    "adaptive-pathway-mcp": {
        "dir": PLUGINS_DIR / "adaptive-pathway",
        "spec": "adaptive_pathway_mcp.spec",
        "exe": "adaptive-pathway-mcp",
        "extras": ["mcp"],
    },
    "wasm-math-mcp": {
        "dir": PLUGINS_DIR / "wasm-math-mcp",
        "spec": "wasm_math_mcp.spec",
        "exe": "wasm-math-mcp",
        "extras": [],
    },
    "kitty-docs-web": {
        "dir": PLUGINS_DIR / "kitty-docs-web",
        "spec": "kitty_docs_web.spec",
        "exe": "kitty-docs-web",
        "extras": [],
    },
    "kitty-tools": {
        "dir": PLUGINS_DIR / "kitty-tools",
        "exe": "kitty-tools",
        "extras": [],
        "kind": "rust",
    },
    "bigtiny": {
        "dir": PLUGINS_DIR / "bigtiny",
        "spec": "bigtiny_daemon.spec",
        "exe": "bigtiny-daemon",
        "extras": [],
    },
}


def run(cmd: list[str], cwd: Path) -> None:
    print(f"$ {' '.join(cmd)}  (in {cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def build_plugin(name: str) -> None:
    if name not in PLUGINS:
        raise SystemExit(f"unknown plugin: {name} (known: {', '.join(PLUGINS)})")
    cfg = PLUGINS[name]
    plugin_dir: Path = cfg["dir"]  # type: ignore[assignment]
    exe_name: str = cfg["exe"]  # type: ignore[assignment]
    kind: str = cfg.get("kind", "python")  # type: ignore[assignment]
    if not plugin_dir.exists():
        raise SystemExit(f"{plugin_dir} not found")

    print(f"\n=== {name} ({kind}) ===")

    if kind == "rust":
        built_exe = build_rust_plugin(plugin_dir, exe_name)
    else:
        built_exe = build_python_plugin(plugin_dir, cfg)

    if not built_exe.exists():
        raise SystemExit(f"expected build output at {built_exe}, but it's missing")

    BINARIES_DIR.mkdir(parents=True, exist_ok=True)
    dest = BINARIES_DIR / f"{exe_name}-{TARGET_TRIPLE}.exe"
    shutil.copy2(built_exe, dest)
    print(f"-> {dest}")


def build_python_plugin(plugin_dir: Path, cfg: dict[str, object]) -> Path:
    spec_file: str = cfg["spec"]  # type: ignore[assignment]
    exe_name: str = cfg["exe"]  # type: ignore[assignment]
    extras: list[str] = cfg["extras"]  # type: ignore[assignment]
    if not (plugin_dir / spec_file).exists():
        raise SystemExit(f"{plugin_dir / spec_file} not found")

    # Install the plugin's own pinned dependencies into whatever Python
    # environment is running this script, so PyInstaller's import analysis
    # can see them. `-e` keeps this reusable during local dev without a
    # reinstall on every source edit.
    target = f".[{','.join(extras)}]" if extras else "."
    run([sys.executable, "-m", "pip", "install", "-e", target], cwd=plugin_dir)
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

    return plugin_dir / "dist" / f"{exe_name}.exe"


def build_rust_plugin(plugin_dir: Path, exe_name: str) -> Path:
    if not (plugin_dir / "Cargo.toml").exists():
        raise SystemExit(f"{plugin_dir / 'Cargo.toml'} not found")
    run(["cargo", "build", "--release", "--locked"], cwd=plugin_dir)
    return plugin_dir / "target" / "release" / f"{exe_name}.exe"


def main() -> None:
    names = sys.argv[1:] or list(PLUGINS)
    for name in names:
        build_plugin(name)
    print(f"\nDone. Frozen binaries are in {BINARIES_DIR}")


if __name__ == "__main__":
    main()
