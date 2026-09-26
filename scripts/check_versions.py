#!/usr/bin/env python3
"""Fail unless every Kitty-owned version number is the same.

Kitty ships in lockstep: the app, its three bundled MCP plugins and the two
memory engines linked into the daemon all carry the app's version. BigTinyV2
is deliberately excluded -- it is shared with other apps and versions on its
own 2.x line.

Usage: python scripts/check_versions.py [--set X.Y.Z]
  --set rewrites every file to X.Y.Z (the release bump), then re-checks.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CARGO_TOMLS = [
    "src-tauri/Cargo.toml",
    "plugins/kitty-tools/Cargo.toml",
    "plugins/kitty-web/Cargo.toml",
    "plugins/kitty-wasm/Cargo.toml",
    "plugins/adaptive-pathway_rust/Cargo.toml",
    "plugins/memorabilia_rust/Cargo.toml",
]
JSON_FILES = ["package.json", "src-tauri/tauri.conf.json"]

# The first `version = "..."` inside [package] -- never a dependency's.
PKG_VERSION = re.compile(r'(\[package\][^\[]*?\nversion\s*=\s*")([^"]+)(")', re.S)
JSON_VERSION = re.compile(r'("version"\s*:\s*")[^"]+(")')


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _write(path: Path, text: str) -> None:
    # Always LF: the repo's .gitattributes normalises to LF, and on Windows
    # plain text-mode writes would otherwise turn every line into CRLF.
    path.write_text(text, encoding="utf-8", newline="\n")


def read_versions() -> dict[str, str]:
    found: dict[str, str] = {}
    for rel in CARGO_TOMLS:
        m = PKG_VERSION.search(_read(ROOT / rel))
        if not m:
            sys.exit(f"{rel}: no [package] version")
        found[rel] = m.group(2)
    for rel in JSON_FILES:
        found[rel] = json.loads(_read(ROOT / rel))["version"]
    return found


def set_version(version: str) -> None:
    for rel in CARGO_TOMLS:
        path = ROOT / rel
        _write(path, PKG_VERSION.sub(lambda m: m.group(1) + version + m.group(3), _read(path), 1))
    for rel in JSON_FILES:
        path = ROOT / rel
        # Rewritten in place rather than re-serialised, so key order and
        # formatting stay exactly as the file had them.
        _write(path, JSON_VERSION.sub(lambda m: m.group(1) + version + m.group(2), _read(path), 1))


def main() -> int:
    args = sys.argv[1:]
    if args[:1] == ["--set"] and len(args) == 2:
        set_version(args[1])
    elif args:
        sys.exit(__doc__)
    found = read_versions()
    distinct = set(found.values())
    if len(distinct) == 1:
        print(f"All Kitty-owned versions are {distinct.pop()}.")
        return 0
    print("Kitty-owned versions disagree:")
    for rel, v in found.items():
        print(f"  {v:>10}  {rel}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
