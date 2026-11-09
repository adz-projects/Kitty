#!/usr/bin/env python3
"""Build every bundled binary (the three MCP plugins and the BigTiny daemon)
and stage it in src-tauri/binaries/ under Tauri's externalBin target-triple
name, so `tauri build` bundles it.

Usage:
    python plugins/build.py                    # build every target
    python plugins/build.py <name>...          # build only the named target(s)
    python plugins/build.py --verify-manifest  # check committed binaries match source

Every target is Rust (`cargo build --release --locked`). The PyInstaller path
that used to live here was removed once the last Python plugin was ported.

**Source manifest.** The built binaries are committed (via Git LFS), which
means a source change can land without anyone rebuilding the binary that ships
it. Each build therefore records, in `src-tauri/binaries/manifest.json`, a
hash of the git-tracked source that went into every binary: the crate's own
directory plus every path dependency, transitively, plus its `Cargo.lock`.
`--verify-manifest` recomputes those hashes and fails if any differs, which is
what CI runs. Line endings are normalised before hashing so a Windows checkout
with CRLF working files agrees with a Linux one.

**Desktop lane only, deliberately.** The target triple is Windows and every
output is an `.exe`, because `externalBin` sidecars are a desktop hosting
shape: Android 10+ refuses to `exec()` a binary in app-writable storage. The
Android build links the same code in-process instead -- the daemon via
`bigtiny2::run` (`src-tauri/src/lifecycle/bigtiny_embedded.rs`) and the MCP
servers with `transport: "in_process"` -- and `tauri.android.conf.json` clears
`bundle.externalBin`. Do not add an Android triple here; a plugin that needs
to run on Android needs an in-process entry point instead (docs/PLUGINS.md).
"""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

TARGET_TRIPLE = "x86_64-pc-windows-msvc"

PLUGINS_DIR = Path(__file__).resolve().parent
REPO_ROOT = PLUGINS_DIR.parent
BINARIES_DIR = REPO_ROOT / "src-tauri" / "binaries"
RESOURCES_DIR = REPO_ROOT / "src-tauri" / "resources"
MANIFEST = BINARIES_DIR / "manifest.json"

# The LiteRT runtime the daemon loads by bare name at runtime; they must sit
# beside it, which Tauri's `bundle.resources` arranges (docs/RELEASE.md,
# "LiteRT runtime"). `litert-lm-rust`'s build script downloads them into its
# own build-script output (`target/release/build/litert-lm-rust-*/out/
# prebuilt/`), and a daemon build stages them from there.
LITERT_DLLS = [
    "libLiteRt.dll",
    "libLiteRtWebGpuAccelerator.dll",
    "libLiteRtTopKWebGpuSampler.dll",
    "libwebgpu_dawn.dll",
    "libGemmaModelConstraintProvider.dll",
    "litert-lm.dll",
]

# The LiteRT-LM runtime with the embedding API (EmbeddingGemma 2's `.litertlm`
# bundle; `BigTinyV2/daemon/src/litert/lm_embedder.rs`). It is a newer runtime
# than the summarizer's `litert-lm.dll` above, whose C API is incompatible, so
# it ships beside it under its own name. Google publishes Windows/Android
# builds only inside the `litert-lm-api` wheels on PyPI; this pins one by its
# PyPI sha256 and takes the DLL out of it. The wheel imports nothing beyond
# Windows system DLLs for CPU use (the GPU compilers in the wheel load on
# demand), so the DLL alone is enough.
LITERT_LM_EMBED_WHEEL = {
    "url": (
        "https://files.pythonhosted.org/packages/4f/73/"
        "c61db42a59c6d540f3bf04616bfcccba172980e62937b8af5bfe8e32c0c3/"
        "litert_lm_api-0.19.1-py3-none-win_amd64.whl"
    ),
    "sha256": "9c5856fbf6dc47fdd29d472b7e0b82e071671e95e02a8cb9fd651a8c8c156367",
    "member": "litert_lm/litert-lm.dll",
    "dest": "litert-lm-embed.dll",
}

# name -> {dir, exe, features}. `exe` is the Cargo [[bin]] name and also the
# externalBin stem `tauri.conf.json` expects.
TARGETS: dict[str, dict[str, object]] = {
    "kitty-tools": {"dir": PLUGINS_DIR / "kitty-tools", "exe": "kitty-tools", "features": []},
    "kitty-web": {"dir": PLUGINS_DIR / "kitty-web", "exe": "kitty-web", "features": []},
    "kitty-wasm": {"dir": PLUGINS_DIR / "kitty-wasm", "exe": "kitty-wasm", "features": []},
    # BigTinyV2 (`BigTinyV2/daemon`), not the frozen V1 tree in
    # `plugins/bigtiny_rust`: desktop Kitty attaches to a shared multi-app V2
    # daemon (`src-tauri/src/lifecycle/bigtiny_v2.rs`). The distinct exe name
    # keeps it from ever being confused with a V1 process.
    #
    # `litert-engine` gives the Windows daemon both local roles: LiteRT
    # embeddings for memory and generative compaction summarization. No native
    # source build is involved -- `litert-lm-rust` downloads prebuilt DLLs.
    "bigtiny": {
        "dir": REPO_ROOT / "BigTinyV2" / "daemon",
        "exe": "bigtiny2-daemon",
        "features": ["litert-engine"],
        "stage_dlls": True,
    },
}

LFS_POINTER_PREFIX = b"version https://git-lfs.github.com/spec/"


def run(cmd: list[str], cwd: Path) -> None:
    print(f"$ {' '.join(cmd)}  (in {cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def is_lfs_pointer(path: Path) -> bool:
    """A git-lfs pointer file: what a clone without git-lfs checks out in place
    of the real binary. Small enough to pass any existence check, so it has to
    be looked for explicitly."""
    try:
        with path.open("rb") as f:
            head = f.read(len(LFS_POINTER_PREFIX))
    except OSError:
        return False
    return head == LFS_POINTER_PREFIX


# --- source manifest -------------------------------------------------------

PATH_DEP = re.compile(r'^\s*[\w-]+\s*=\s*\{[^}]*\bpath\s*=\s*"([^"]+)"', re.M)


def crate_dirs(root: Path) -> list[Path]:
    """`root` plus every crate it reaches through `path = ...` dependencies,
    transitively. A crate pointing at itself (as the daemon's dev-dependency
    on itself does) is harmless: it is already in the set."""
    seen: list[Path] = []
    stack = [root.resolve()]
    while stack:
        d = stack.pop()
        if d in seen:
            continue
        seen.append(d)
        toml = (d / "Cargo.toml").read_text(encoding="utf-8")
        for rel in PATH_DEP.findall(toml):
            dep = (d / rel).resolve()
            if (dep / "Cargo.toml").exists():
                stack.append(dep)
    return sorted(seen)


def tracked_files(dirs: list[Path]) -> list[Path]:
    rels = [str(d.relative_to(REPO_ROOT)).replace("\\", "/") for d in dirs]
    out = subprocess.run(
        ["git", "ls-files", "-z", "--", *rels],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
    ).stdout
    return sorted(REPO_ROOT / p for p in out.decode("utf-8").split("\0") if p)


def source_hash(name: str) -> str:
    cfg = TARGETS[name]
    files = tracked_files(crate_dirs(cfg["dir"]))  # type: ignore[arg-type]
    h = hashlib.sha256()
    for f in files:
        rel = str(f.relative_to(REPO_ROOT)).replace("\\", "/")
        data = f.read_bytes().replace(b"\r\n", b"\n")
        h.update(rel.encode("utf-8") + b"\0" + hashlib.sha256(data).digest())
    return h.hexdigest()


def load_manifest() -> dict[str, str]:
    if not MANIFEST.exists():
        return {}
    return json.loads(MANIFEST.read_text(encoding="utf-8"))


def save_manifest(manifest: dict[str, str]) -> None:
    text = json.dumps(dict(sorted(manifest.items())), indent=2) + "\n"
    MANIFEST.write_text(text, encoding="utf-8", newline="\n")


def verify_manifest() -> int:
    manifest = load_manifest()
    problems: list[str] = []
    for name, cfg in TARGETS.items():
        exe = BINARIES_DIR / f"{cfg['exe']}-{TARGET_TRIPLE}.exe"
        if not exe.exists():
            problems.append(f"{name}: {exe.name} is missing")
        elif is_lfs_pointer(exe):
            problems.append(f"{name}: {exe.name} is a git-lfs pointer, not a binary (run `git lfs pull`)")
        expected = source_hash(name)
        recorded = manifest.get(name)
        if recorded is None:
            problems.append(f"{name}: not in {MANIFEST.name}; rebuild it with `python plugins/build.py {name}`")
        elif recorded != expected:
            problems.append(
                f"{name}: its source changed since the committed binary was built; "
                f"rebuild it with `python plugins/build.py {name}` and commit the result"
            )
    for res in sorted(RESOURCES_DIR.iterdir()):
        if res.is_file() and is_lfs_pointer(res):
            problems.append(f"resources/{res.name} is a git-lfs pointer, not the file (run `git lfs pull`)")
    if problems:
        print("Bundled binaries are not in a shippable state:")
        for p in problems:
            print(f"  - {p}")
        return 1
    print("Bundled binaries match their source.")
    return 0


# --- building --------------------------------------------------------------


def build_target(name: str) -> None:
    if name not in TARGETS:
        raise SystemExit(f"unknown target: {name} (known: {', '.join(TARGETS)})")
    cfg = TARGETS[name]
    crate_dir: Path = cfg["dir"]  # type: ignore[assignment]
    exe_name: str = cfg["exe"]  # type: ignore[assignment]
    features: list[str] = cfg["features"]  # type: ignore[assignment]
    print(f"\n=== {name} ===")

    cmd = ["cargo", "build", "--release", "--locked"]
    if features:
        cmd += ["--features", ",".join(features)]
    run(cmd, cwd=crate_dir)
    release_dir = crate_dir / "target" / "release"
    built = release_dir / f"{exe_name}.exe"
    if not built.exists():
        raise SystemExit(f"expected build output at {built}, but it's missing")

    BINARIES_DIR.mkdir(parents=True, exist_ok=True)
    dest = BINARIES_DIR / f"{exe_name}-{TARGET_TRIPLE}.exe"
    shutil.copy2(built, dest)
    print(f"-> {dest}")

    if cfg.get("stage_dlls"):
        prebuilt = litert_prebuilt_dir(release_dir)
        for dll in LITERT_DLLS:
            src = prebuilt / dll
            if not src.exists():
                raise SystemExit(f"{dll} not found in {prebuilt}; the LiteRT download step did not run")
            shutil.copy2(src, RESOURCES_DIR / dll)
            print(f"-> {RESOURCES_DIR / dll}")
        stage_litert_lm_embed()

    manifest = load_manifest()
    manifest[name] = source_hash(name)
    save_manifest(manifest)


def stage_litert_lm_embed() -> None:
    """Fetch the pinned `litert-lm-api` wheel and stage its DLL as
    `litert-lm-embed.dll`. Skipped when the staged file already came from
    this wheel (the wheel's hash is recorded beside it)."""
    import io
    import urllib.request
    import zipfile

    spec = LITERT_LM_EMBED_WHEEL
    dest = RESOURCES_DIR / spec["dest"]
    stamp = RESOURCES_DIR / (spec["dest"] + ".source")
    if dest.exists() and not is_lfs_pointer(dest) and stamp.exists() and stamp.read_text().strip() == spec["sha256"]:
        print(f"-> {dest} (up to date)")
        return
    print(f"fetching {spec['url']}")
    with urllib.request.urlopen(spec["url"]) as r:
        data = r.read()
    got = hashlib.sha256(data).hexdigest()
    if got != spec["sha256"]:
        raise SystemExit(f"litert-lm-api wheel hash mismatch: got {got}, pinned {spec['sha256']}")
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        dest.write_bytes(z.read(spec["member"]))
    stamp.write_text(spec["sha256"] + "\n")
    print(f"-> {dest}")


def litert_prebuilt_dir(release_dir: Path) -> Path:
    """Where `litert-lm-rust` put the DLLs it downloaded for this build.

    One `build/litert-lm-rust-<hash>` directory per feature/profile
    combination can exist side by side; the one this build used is the one
    whose runtime DLL was written most recently.
    """
    candidates = [
        d / "out" / "prebuilt"
        for d in (release_dir / "build").glob("litert-lm-rust-*")
        if (d / "out" / "prebuilt" / LITERT_DLLS[0]).exists()
    ]
    if not candidates:
        raise SystemExit(
            f"no litert-lm-rust download under {release_dir / 'build'}; "
            "was the daemon built with --features litert-engine?"
        )
    return max(candidates, key=lambda d: (d / LITERT_DLLS[0]).stat().st_mtime)


def main() -> int:
    args = sys.argv[1:]
    if args == ["--verify-manifest"]:
        return verify_manifest()
    for name in args or list(TARGETS):
        build_target(name)
    print(f"\nDone. Binaries are in {BINARIES_DIR}; {MANIFEST.name} updated.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
