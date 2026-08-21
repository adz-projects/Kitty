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

### LiteRT runtime (the Windows daemon's local engine)

The Windows daemon is built with `--features litert-engine` (embeddings +
generative compaction summarization; there is **no llama.cpp** anymore — that
whole cmake/Vulkan/glslc build surface is gone). Building and shipping it needs
two things beyond the ordinary `cargo build`:

1. **Build the daemon with the feature** (its `build.rs` auto-copies
   `litert-lm.if.lib` → `litert-lm.lib`, so no manual link step):

   ```powershell
   cd plugins/bigtiny_rust
   cargo build --release --features litert-engine --bin bigtiny-daemon
   # → target/release/bigtiny-daemon.exe (~76 MB), then copy to
   #   src-tauri/binaries/bigtiny-daemon-x86_64-pc-windows-msvc.exe
   ```

2. **Bundle the LiteRT native DLLs + the Gemma tokenizer beside the daemon.**
   `litert-lm-rust`'s `download-native` fetches these into the crate's build
   output (`$CARGO_TARGET_DIR/release/`); collect the **six** DLLs:

   - `libLiteRt.dll`
   - `libLiteRtWebGpuAccelerator.dll`
   - `libLiteRtTopKWebGpuSampler.dll`
   - `libwebgpu_dawn.dll`
   - `libGemmaModelConstraintProvider.dll`
   - `litert-lm.dll`

   These must land in the **same directory as `bigtiny-daemon.exe`** at
   runtime, because the daemon loads `libLiteRt.dll` by bare name
   (`Library::from_path("libLiteRt.dll")`, resolved by the OS from the loading
   process's own directory) and that DLL pulls in the other five. Tauri's
   `externalBin` places the daemon in the install root and `resource_dir()`
   resolves to that same directory on Windows, so bundling the DLLs via
   `bundle.resources` co-locates them with the daemon. Add them to
   `src-tauri/tauri.conf.json` `bundle.resources` (stage the files under, e.g.,
   `src-tauri/resources/litert/` and reference that glob) — `tauri build` errors
   on a missing resource path, so the files must be present before you add the
   entry.

   The **Gemma `tokenizer.json`** the embedder needs is likewise bundled as a
   resource (the `litert-community` repo ships only `sentencepiece.model`; the
   canonical `tokenizer.json` comes from the gated `google/embeddinggemma-300m`
   and is converted/vendored once, offline — it is **not** downloaded at
   runtime). `lifecycle/bigtiny_env.rs` resolves it via `resource_dir()` and
   passes `BIGTINY_LITERT__TOKENIZER_PATH`.

> **Binary shipping is an open decision.** The daemon (76 MB) + six DLLs
> (~78 MB) + tokenizer.json (~33 MB) exceed GitHub's 100 MB non-LFS file limit
> in aggregate and individually push the repo large. Either migrate
> `src-tauri/binaries/` + the LiteRT resources to **Git LFS**, or keep them out
> of git and stage them from a release asset before `tauri build`. Decide this
> before the next tagged release; the committed daemon binary is a placeholder
> either way (see `src-tauri/binaries/README.md`).

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
2. Re-verify the curated **LiteRT** repos/filenames in
   `src/lib/curated_models.ts` still resolve on HuggingFace: EmbeddingGemma
   `.tflite` (gated — needs an accepted Gemma license + HF token) on both
   platforms, and `gemma-4-E2B-it.litertlm` for the Windows summarizer. A
   renamed repo or a moved license gate turns first run into a dead end.
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

The Android build is now **much lighter than the llama.cpp era**: the local
engine is LiteRT (`litert-embed` feature — embeddings only; no generative model
runs on the phone), which is pure `libloading` + pure-Rust `tokenizers` with
**no native build at all**. That removes every cmake / Ninja / Vulkan SDK /
`glslc` / SPIRV-Headers / MSVC-shell requirement the old llama.cpp cross-compile
carried. The `libLiteRt.so` comes from the Google Maven AAR, not a source build
(see below). What remains:

```powershell
$ndk = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.2.12479018"
$env:ANDROID_HOME      = "$env:LOCALAPPDATA\Android\Sdk"
$env:ANDROID_NDK_HOME  = $ndk
$env:CARGO_TARGET_DIR  = "C:\kt-android"   # keep the Rust build off the long repo path
```

`cargo-ndk` (`cargo install cargo-ndk`) is required to cross-compile the Rust
cdylib for `aarch64-linux-android`. `cmake`/`ninja`/the Vulkan SDK are **no
longer needed** for the Kitty build.

**LiteRT `libLiteRt.so` (Android).** `app/build.gradle.kts` consumes the Google
Maven AAR `com.google.ai.edge.litert:litert:2.1.4` as a **file-only**
configuration (`litertAar`, `isTransitive = false`) and a `Copy` task
(`extractLiteRtJni`, wired to `preBuild`) unzips `jni/**/libLiteRt.so` into a
generated `jniLibs` dir that the `main` source set includes. AGP then merges it
into the APK like our own `libkitty_lib.so`, and the Rust embedder loads it by
name at runtime. The AAR is **not** put on the compile classpath
(`implementation`): its Kotlin API metadata (2.3.0) is incompatible with the
project's Kotlin 1.9 and breaks the Kotlin compile — we only want the `.so`.

The Gemma `tokenizer.json` is bundled the same way it is on Windows (an app
resource, converted offline from `sentencepiece.model`); Android has no
generative summarizer, so no `.litertlm` and none of the Windows DLLs ship in
the AAB.

### Build

```powershell
pnpm tauri android build --target aarch64          # AAB, release variant
pnpm tauri android build --apk --target aarch64    # APK, for sideloading
pnpm tauri android build --apk --debug --target aarch64   # debug APK
```

**`--target aarch64` is not optional.** Without it the CLI builds a *universal*
APK — all four ABIs — and `armeabi-v7a` fails: `edgefirst-tflite-sys` does not
compile for 32-bit (25 × `E0080` const-eval errors on pointer width). That ABI
is not shipped anyway; D22 in `docs/ANDROID.md` pins `aarch64-linux-android` as
the only v1 target. Verified 2026-08-21: the bare `--apk` form gets through the
aarch64 work and then dies on armv7 after ~8 minutes.

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
