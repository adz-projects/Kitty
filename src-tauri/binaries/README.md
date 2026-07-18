# externalBin placeholders

`adaptive-pathway-sidecar-x86_64-pc-windows-msvc.exe` and
`replacement-mcp-x86_64-pc-windows-msvc.exe` in this directory are **empty
placeholder files**, committed only so `cargo build`/`cargo check`/`cargo
test` succeed on a fresh clone — Tauri's build script validates that every
`bundle.externalBin` entry (see `../tauri.conf.json`) exists on disk at
build time, even for a plain `cargo build`, not just packaging.

Before an actual release build (`tauri build` / `pnpm tauri build`), run:

```
python plugins/build.py
```

from the repo root. It freezes both plugins with PyInstaller and overwrites
these placeholders with the real executables. Packaging with the
placeholders still in place would produce an app that can't actually start
either plugin — `tauri build` doesn't distinguish a placeholder from a real
binary, only that the file exists.
