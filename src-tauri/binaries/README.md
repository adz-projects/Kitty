# externalBin placeholders

Any `*-x86_64-pc-windows-msvc.exe` in this directory that is **zero bytes**
is an empty placeholder, committed only so `cargo build`/`cargo check`/`cargo
test` succeed on a fresh clone — Tauri's build script validates that every
`bundle.externalBin` entry (see `../tauri.conf.json`) exists on disk at
build time, even for a plain `cargo build`, not just packaging.

Before an actual release build (`tauri build` / `pnpm tauri build`), run:

```
python plugins/build.py
```

from the repo root. It freezes every plugin with PyInstaller and overwrites
any placeholders with the real executables. Packaging with a placeholder
still in place would produce an app that can't actually start that plugin —
`tauri build` doesn't distinguish a placeholder from a real binary, only
that the file exists.
