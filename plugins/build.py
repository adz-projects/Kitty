#!/usr/bin/env python3
"""Freezes every internal plugin (and the BigTiny daemon) to a standalone
Windows .exe via PyInstaller and drops the result into src-tauri/binaries/
with Tauri's externalBin target-triple naming convention, so `tauri build`
bundles them automatically.

Usage:
    python plugins/build.py            # build every target
    python plugins/build.py <name>...  # build only the named target(s)

Each Python target is built inside its own fresh venv
(`plugins/<name>/.build-venv/`, gitignored), created from scratch on every
run rather than reused with `pip install -e .` on top of whatever was there
before. PyInstaller's import analysis follows *any* resolvable `import`
statement it finds by static scanning — including ones inside
`try/except ImportError` blocks that are never actually reached at runtime
— so if the interpreter running this script happens to have unrelated,
much heavier packages installed (a different project's ML stack, say),
those can get pulled into the frozen exe even though the plugin never uses
them. A venv containing only this one plugin's own pinned dependencies
makes that structurally impossible: a package PyInstaller can't `import`
during analysis is one it can't accidentally bundle. This is also why the
old `sys.executable`-based approach's own docstring recommended "a
dedicated venv, not your system Python" — that recommendation is now
enforced instead of just suggested.

Requires each target's own dependencies as declared in its own
`pyproject.toml`; PyInstaller itself is installed into each venv
automatically. `python3`/`py -3` must be resolvable on PATH to create the
per-plugin venvs.

Windows-only app, so the target triple is hardcoded to the one Kitty ships
(see docs/VERSIONS.md).
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import venv
from pathlib import Path

TARGET_TRIPLE = "x86_64-pc-windows-msvc"

PLUGINS_DIR = Path(__file__).resolve().parent
REPO_ROOT = PLUGINS_DIR.parent
BINARIES_DIR = REPO_ROOT / "src-tauri" / "binaries"
BUILD_VENV_DIRNAME = ".build-venv"

# name -> { dir, spec, exe, extras, kind }. `exe` must match pyproject.toml's
# console script name (Python) or Cargo.toml's [[bin]] name (Rust), since
# that's also what the Rust side's launch-command override expects. `extras`
# are optional-dependency-group names installed alongside the base package
# (`pip install -e ".[extra1,extra2]"`) — ignored for `kind: "rust"`. `kind`
# defaults to `"python"` (see `build_plugin`) so every pre-existing entry is
# unchanged; only `kitty-tools` sets it to `"rust"`.
#
# `replacement-mcp`, `brave-mcp-search`, `visualizations`, `kitty-docs-web`,
# and `wasm-math-mcp` are retired — their tools are all hosted inside
# `kitty-tools` (Rust), `kitty-web` (Rust), and `kitty-wasm` (Rust) now (the
# Rust consolidation) — and deliberately absent from this dict, so
# `python plugins/build.py` (no args) no longer builds or bundles them.
# Their source files stay in-tree, unbuilt, as the oracle for re-verifying
# the Rust ports against if a behavioral gap ever surfaces (see
# docs/PLUGINS.md).
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
    "kitty-tools": {
        "dir": PLUGINS_DIR / "kitty-tools",
        "exe": "kitty-tools",
        "extras": [],
        "kind": "rust",
    },
    # The Rust replacements for `replacement-mcp`'s tool set: `kitty-tools`
    # (local tools + Excel/PDF) and `kitty-web` (web search/scrape), plus
    # `kitty-wasm` (which supersedes the now-retired `wasm-math-mcp` Python
    # plugin). Each is still built and bundled as a stdio server here
    # (desktop's hosting shape); a host that can't exec() a bundled binary
    # links them in-process instead via `bigtiny_rust::mcp::builtin` — see
    # docs/PLUGINS.md.
    "kitty-web": {
        "dir": PLUGINS_DIR / "kitty-web",
        "exe": "kitty-web",
        "extras": [],
        "kind": "rust",
    },
    "kitty-wasm": {
        "dir": PLUGINS_DIR / "kitty-wasm",
        "exe": "kitty-wasm",
        "extras": [],
        "kind": "rust",
    },
    # Backed by the pure-Rust rewrite (`plugins/bigtiny_rust/`), not the
    # original Python daemon vendored at `plugins/bigtiny/` — that source
    # tree stays in-tree, unbuilt, as the behavioral oracle the Rust port
    # was verified against (same convention as `replacement-mcp` et al. per
    # the module doc comment above). `exe` stays "bigtiny-daemon" so every
    # bundling/lifecycle path that already looks for that filename
    # (`config::default_bigtiny_command`, `lifecycle::bigtiny_proc`) needs
    # no changes.
    "bigtiny": {
        "dir": PLUGINS_DIR / "bigtiny_rust",
        "exe": "bigtiny-daemon",
        "extras": [],
        "kind": "rust",
    },
}


def run(cmd: list[str], cwd: Path) -> None:
    print(f"$ {' '.join(cmd)}  (in {cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def fresh_venv_python(plugin_dir: Path) -> Path:
    """(Re)create `plugin_dir/.build-venv` from scratch and return its
    python.exe. Always wiped and rebuilt rather than reused: the whole point
    is that this environment only ever contains what *this* build just
    installed into it, never anything left over from a previous plugin or
    a previous version of this one's dependencies."""
    venv_dir = plugin_dir / BUILD_VENV_DIRNAME
    if venv_dir.exists():
        shutil.rmtree(venv_dir)
    print(f"$ python -m venv {venv_dir}")
    # with_pip=True (default) gives us a venv-local pip to install into,
    # isolated from whatever's on the interpreter running this script.
    venv.create(venv_dir, with_pip=True)
    python = venv_dir / "Scripts" / "python.exe"
    if not python.exists():
        raise SystemExit(f"venv creation didn't produce {python}")
    return python


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

    # Build in a throwaway venv containing only this plugin's own pinned
    # dependencies (see `fresh_venv_python`'s doc comment for why: PyInstaller
    # bundles anything it can statically resolve an `import` for, including
    # dead try/except-ImportError branches, so a package merely being
    # *installed* in the build environment — even one this plugin never
    # actually imports — is enough for it to get pulled into the frozen exe).
    python = fresh_venv_python(plugin_dir)
    target = f".[{','.join(extras)}]" if extras else "."
    run([str(python), "-m", "pip", "install", "-e", target], cwd=plugin_dir)
    run([str(python), "-m", "pip", "install", "pyinstaller"], cwd=plugin_dir)

    run(
        [
            str(python),
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
