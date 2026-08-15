# Release checklist

Kitty ships on two targets: **Windows** (NSIS installer, the mature one) and
**Android** (AAB). They share the Rust core and the entire frontend; they do
not share a packaging story, so each gets its own section below.

## Windows

### Build

```powershell
python plugins/build.py     # build the four bundled binaries into
                            # src-tauri/binaries/ — see plugins/README.md.
                            # Skipping this step leaves the committed empty
                            # placeholders in place, which `tauri build` will
                            # happily bundle without complaint but which can't
                            # actually run.
pnpm install
pnpm tauri build            # release build + NSIS installer
```

All four targets are now Rust (`cargo build --release`, no PyInstaller and no
Python runtime involved): `bigtiny` (the daemon, which statically links the
behavioral-memory engine at `plugins/adaptive-pathway_rust/`), `kitty-tools`,
`kitty-web`, and `kitty-wasm`. The Python `adaptive-pathway` sidecar and its
`adaptive-pathway-mcp` proxy are retired and no longer built or bundled.

`plugins/build.py` still drives it, purely because it owns the
target-triple naming convention `externalBin` expects.

Artifacts:

- `src-tauri/target/release/kitty.exe`
- `src-tauri/target/release/bundle/nsis/Kitty_<version>_x64-setup.exe`

### Signing (placeholder)

Code signing is **not yet configured** (a deliberate choice for now, not an
oversight). Before public distribution, obtain an Authenticode certificate and
set Tauri's `bundle.windows.certificateThumbprint` (or `signCommand`) so the
exe + NSIS installer are signed; otherwise SmartScreen warns on first run.

Until then, expect: installing or first-running the unsigned `...-setup.exe`
(or the installed `kitty.exe` itself) shows Windows SmartScreen's "Windows
protected your PC" warning. Users click **More info** → **Run anyway** to
proceed — this is expected, not a build failure.

### Version bump

1. Bump `version` in `package.json` **and** `src-tauri/tauri.conf.json`
   (and `src-tauri/Cargo.toml`) — keep them in sync.
2. Re-verify the curated GGUF repos/revisions in `src/lib/curated_models.ts`
   still resolve on HuggingFace, including the support models the Android
   wizard downloads (summarizer + embedding). A renamed or re-quantized repo
   turns first run into a dead end.
   Also bump `tauri.android.versionCode` — Play rejects a re-used one.
3. Re-verify all four `plugins/build.py` targets still build cleanly. (The
   pinned Python + PyInstaller versions in `docs/VERSIONS.md` no longer gate
   this — every target is Rust now — but `plugins/build.py` itself is still a
   Python script and needs an interpreter on PATH.)
4. If BigTiny's own API surface changed, re-check the route shapes assumed
   in `src-tauri/src/bigtiny/` (`client.rs`, `sessions.rs`, `stream.rs`,
   `providers.rs`, `mcp.rs`) against BigTiny's current API.md.

### Pre-release verification

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
  `bigtiny-daemon` children (we kill only processes we spawned).

---

## Android

### Toolchain

Everything in `docs/ANDROID.md` §11 is required, and every one of these fails
in a way that reads like a broken crate rather than a missing tool. Set them in
the shell you build from:

```powershell
$ndk = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.2.12479018"
$env:ANDROID_HOME      = "$env:LOCALAPPDATA\Android\Sdk"
$env:ANDROID_NDK_HOME  = $ndk
# llama-cpp-sys-2's build.rs reads these three, NOT ANDROID_NDK_HOME:
$env:ANDROID_NDK       = $ndk
$env:ANDROID_NDK_ROOT  = $ndk
$env:NDK_HOME          = $ndk
$env:LIBCLANG_PATH     = "$env:LOCALAPPDATA\kitty-buildtools\libclang"
$env:CMAKE_GENERATOR   = "Ninja"   # else cmake picks MSBuild and builds x64
$env:CARGO_TARGET_DIR  = "C:\kt"   # cl.exe hits MAX_PATH from the repo path
# --- Vulkan GPU backend on Android (ADDENDUM 3) ---
# The arm64 build enables llama.cpp's `vulkan` feature. From a Windows host it
# needs the Vulkan SDK's C++ headers + SPIRV-Headers + glslc, AND it must be run
# from an MSVC shell so the host `vulkan-shaders-gen` sub-build finds `cl.exe`
# (a bare shell falls to Git's MinGW gcc, which fails to link). Run the whole
# `pnpm tauri android build` from inside `vcvars64.bat`:
$vk = "C:\VulkanSDK\1.4.357.0"
$env:VULKAN_INCLUDE_DIR       = "$vk\Include"
$env:SPIRV_HEADERS_DIR        = "$vk\Lib\cmake\SPIRV-Headers"
$env:SPIRV_HEADERS_INCLUDE_DIR= "$vk\Include"
$env:VULKAN_GLSLC             = "$vk\Bin\glslc.exe"   # PATH search omits `.exe`
# then, in a shell that has called:
#   & "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
# so `cl.exe` is first on PATH for the host shaders-gen tool.
```

`cmake` and `ninja` must be on `PATH`. After a *failed* configure, delete
`$env:CARGO_TARGET_DIR\<triple>\*\build\llama-cpp-sys-2-*\` before retrying —
the crate logs "already configured, skipping" and reuses the wrong generator
forever otherwise.

### Build

```powershell
pnpm tauri android build          # AAB, release variant
pnpm tauri android build --apk    # APK, for sideloading a test build
```

**`plugins/build.py` is not part of this lane and must not be run for it.**
There are no Android sidecars: `tauri.android.conf.json` clears
`bundle.externalBin`, the daemon is linked in and hosted in-process
(`lifecycle/bigtiny_embedded.rs`), and the three MCP servers register with
`transport: "in_process"`. Android 10+ refuses to `exec()` a binary in
app-writable storage, so a frozen per-plugin executable has nowhere to live.

Artifacts:

- `src-tauri/gen/android/app/build/outputs/bundle/universalRelease/app-universal-release.aab`
- `src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk`

### Signing

The upload key is **not in this repo and must not be**. Create one once:

```powershell
keytool -genkey -v -keystore $env:USERPROFILE\kitty-upload.jks `
  -keyalg RSA -keysize 2048 -validity 10000 -alias kitty-upload
```

Then write `src-tauri/gen/android/keystore.properties` (gitignored):

```properties
storeFile=C:/Users/<you>/kitty-upload.jks
storePassword=...
keyAlias=kitty-upload
keyPassword=...
```

`app/build.gradle.kts` picks it up automatically. **Without it the release
variant builds unsigned** — deliberately: that still verifies the lane end to
end, and Play rejects the artifact, so nothing ships by accident.

### Android-specific verification

- `minSdk` is 26 and must stay ≥ the `--platform` the native library was built
  against, or the app installs and then dies at `System.loadLibrary`.
- 16 KB page size: `src-tauri/.cargo/config.toml` passes
  `-Wl,-z,max-page-size=16384`. Required for Play from Nov 2025. Check with
  `llvm-readelf -l libkitty_lib.so | Select-String LOAD`.
- **A second app on the device cannot reach the daemon** — loopback is not
  process-private on Android. From `adb shell` (the same unprivileged position
  any installed app is in), `curl 127.0.0.1:<port>/api/chat/` must 401.
- Soft keyboard: the header and model picker stay on screen and the composer
  sits on the keyboard (`lib/viewport.ts`).
- Download an artifact and confirm it lands where the file picker said.

### Secrets and long downloads

Both of these were release blockers and are now closed; the notes stay because
each has a verification step that is easy to skip.

- **Provider keys persist (D24, closed).** Not via `keyring` — that crate has
  no Android backend and is now excluded from the target's dependency graph
  entirely so its in-memory mock cannot come back. Secrets are AES-256-GCM
  sealed under a non-exportable AndroidKeyStore key
  (`gen/android/.../SecretStore.kt`, reached from
  `src/android/secrets.rs`). **Verify by relaunching**: save a provider key,
  force-stop the app, reopen it, and confirm the provider still authenticates.
  A mock passes every test that does not cross a process boundary.
- **Downloads survive backgrounding (closed).** A `dataSync` foreground
  service with a partial wake lock brackets the transfer, and transport
  failures resume from the `.part` byte offset with a bounded, progress-aware
  retry budget. **Verify by leaving**: start a model download, switch away,
  lock the screen, and toggle airplane mode mid-transfer — it should recover
  and finish, with the notification tracking it throughout.

### Remaining Android gaps (not blockers)

- The CPython WASI guest is not bundled, so `kitty-wasm`'s Python tools
  download it on first use (`docs/BACKLOG.md`).
