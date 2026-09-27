# Bundled binaries

The engine (`bigtiny2-daemon-*.exe`) and the MCP servers (`kitty-tools`,
`kitty-web`, `kitty-wasm`) Tauri bundles through `bundle.externalBin` (see
`../tauri.conf.json`). They are real builds, committed as **Git LFS** objects:
Tauri's build script checks that every `externalBin` entry exists on disk even
for a plain `cargo build`, so a clone needs them (`git lfs pull`) before it can
build at all. A clone without LFS has small pointer files instead, which
`plugins/build.py --verify-manifest` and CI reject.

`manifest.json` records, per binary, a hash of the source it was built from.
After changing the engine or a plugin:

```
python plugins/build.py [target ...]     # rebuild; updates manifest.json
python plugins/build.py --verify-manifest
```

then commit the rebuilt binary together with `manifest.json`. CI fails when a
committed binary no longer matches its source.
