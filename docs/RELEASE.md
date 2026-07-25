# Release checklist

## Build

```powershell
python plugins/build.py    # freeze the BigTiny daemon (plugins/bigtiny/) +
                            # internal plugins (adaptive-pathway,
                            # adaptive-pathway-mcp, replacement-mcp, etc.) to
                            # src-tauri/binaries/ — see plugins/README.md.
                            # Skipping this step leaves the committed empty
                            # placeholders in place, which `tauri build` will
                            # happily bundle without complaint but which can't
                            # actually run.
pnpm install
pnpm tauri build            # release build + NSIS installer
```

Artifacts:

- `src-tauri/target/release/kitty.exe`
- `src-tauri/target/release/bundle/nsis/Kitty_<version>_x64-setup.exe`

## Signing (placeholder)

Code signing is **not yet configured**. Before public distribution, obtain an
Authenticode certificate and set Tauri's `bundle.windows.certificateThumbprint`
(or `signCommand`) so the exe + NSIS installer are signed; otherwise SmartScreen
warns on first run.

## Version bump

1. Bump `version` in `package.json` **and** `src-tauri/tauri.conf.json`
   (and `src-tauri/Cargo.toml`) — keep them in sync.
2. Re-verify the curated starter model tags on ollama.com
   (`src/lib/starter_models.ts`) and the Ollama installer URL in `docs/VERSIONS.md`.
3. Re-verify the pinned Python + PyInstaller versions in `docs/VERSIONS.md`
   still build all four `plugins/build.py` targets cleanly.
4. If BigTiny's own API surface changed, re-check the route shapes assumed
   in `src-tauri/src/bigtiny/` (`client.rs`, `sessions.rs`, `stream.rs`,
   `providers.rs`, `mcp.rs`) against BigTiny's current API.md.

## Pre-release verification

- `cargo clippy --all-targets` clean, `cargo test`, `pnpm lint`, `pnpm build`.
- `python plugins/build.py` succeeded and `src-tauri/binaries/*.exe` are real
  (non-empty) — the committed placeholders satisfy `cargo build`'s existence
  check but produce a non-functional plugin if actually packaged.
- Secret audit: no secret in any log line, `config.json`, or event payload
  (provider keys live in Windows Credential Manager via `keyring`; BigTiny's
  `X-API-Key` stays in Rust memory + env + the local `BIGTINY_SECRET`).
- Manual smoke: first-run wizard → chat → tool approval → resume a session →
  restart Kitty's engine from the degraded panel and confirm the session rebuilds.
- Soak: repeated summon/dismiss during active streams leaves no orphaned
  `bigtiny-daemon` / `ollama` children (we kill only processes we spawned).
