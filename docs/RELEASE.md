# Release checklist

## Build

```powershell
python plugins/build.py    # freeze the internal plugins (adaptive-pathway,
                            # replacement-mcp) to src-tauri/binaries/ — see
                            # plugins/README.md. Skipping this step leaves the
                            # committed empty placeholders in place, which
                            # `tauri build` will happily bundle without
                            # complaint but which can't actually run.
pnpm install
pnpm tauri build            # release build + NSIS installer
```

Artifacts:

- `src-tauri/target/release/goose-overlay.exe`
- `src-tauri/target/release/bundle/nsis/Goose Overlay_<version>_x64-setup.exe`

## Signing (placeholder)

Code signing is **not yet configured**. Before public distribution, obtain an
Authenticode certificate and set Tauri's `bundle.windows.certificateThumbprint`
(or `signCommand`) so the exe + NSIS installer are signed; otherwise SmartScreen
warns on first run.

## Version bump

1. Bump `version` in `package.json` **and** `src-tauri/tauri.conf.json`
   (and `src-tauri/Cargo.toml`) — keep them in sync.
2. **Re-verify the pinned Goose version** in `docs/VERSIONS.md`: run `goose --version`
   and re-check the ACP surface in [acp-protocol.md](acp-protocol.md) against the
   pinned build (methods drift between versions — `session/*`, `_goose/unstable/*`,
   `session/update` variants). All ACP path assumptions live in
   `src-tauri/src/goosed/` + `docs/acp-protocol.md`.
3. Re-verify the curated starter model tags on ollama.com
   (`src/lib/starter_models.ts`) and the installer URLs in `docs/VERSIONS.md`.
4. Update the stock Goose Desktop process name(s) in `docs/VERSIONS.md`
   (conflict detection) if the desktop app's exe changed.
5. Re-verify the pinned Python + PyInstaller versions in `docs/VERSIONS.md`
   still build both plugins cleanly (`python plugins/build.py`).

## Pre-release verification

- `cargo clippy --all-targets` clean, `cargo test`, `pnpm lint`, `pnpm build`.
- `python plugins/build.py` succeeded and `src-tauri/binaries/*.exe` are real
  (non-empty) — the committed placeholders satisfy `cargo build`'s existence
  check but produce a non-functional plugin if actually packaged.
- Secret audit: no secret in any log line, `config.json`, or event payload
  (provider keys live in Windows Credential Manager via `keyring`; goosed's
  `X-Secret-Key` stays in Rust memory + env + the local WS `token`).
- Manual smoke: first-run wizard → chat → tool approval → resume a session →
  restart Goose from the degraded panel and confirm the session rebuilds.
- Soak: repeated summon/dismiss during active streams leaves no orphaned
  `goose serve` / `ollama` children (we kill only processes we spawned).
