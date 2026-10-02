# Release checklist

Kitty ships on two targets: **Windows** (NSIS installer, the mature one) and
**Android** (AAB). They share the Rust core and the entire frontend; they do
not share a packaging story, so each gets its own section below.

## Windows

### Build

```powershell
git lfs pull                               # the bundled binaries are LFS objects
python plugins/build.py --verify-manifest  # do they match their source?
python plugins/build.py [target ...]       # if not: rebuild, then commit the
                                           # binary with its manifest entry
pnpm install
pnpm tauri build                           # release build + NSIS installer
```

Every target is Rust (`cargo build --release`): `bigtiny` (the engine,
`BigTinyV2/daemon`, which links both memory engines), `kitty-tools`,
`kitty-web` and `kitty-wasm`. `plugins/build.py` owns the target-triple naming
`externalBin` expects, stages the LiteRT DLLs, and writes a source hash per
binary into `src-tauri/binaries/manifest.json`; `--verify-manifest` (also run
in CI) fails when a committed binary is older than its source or is an LFS
pointer.

Artifacts:

- `src-tauri/target/release/kitty.exe`
- `src-tauri/target/release/bundle/nsis/Kitty_<version>_x64-setup.exe`

### LiteRT runtime (the Windows daemon's local engine)

The Windows daemon is built with `--features litert-engine` (embeddings +
generative compaction summarization; there is **no llama.cpp** anymore — that
whole cmake/Vulkan/glslc build surface is gone). Building and shipping it needs
two things beyond the ordinary `cargo build`:

1. **Build the daemon with the feature** (its `build.rs` auto-copies
   `litert-lm.if.lib` → `litert-lm.lib`, so no manual link step).
   `plugins/build.py` already does this — it is the source of truth for the
   crate path, the feature list and the destination filename, so prefer it
   over the manual form and change it rather than this document if any of
   those move:

   ```powershell
   python plugins/build.py bigtiny
   # runs, in BigTinyV2/daemon:
   #   cargo build --release --locked --features litert-engine
   # → copied to
   #   src-tauri/binaries/bigtiny2-daemon-x86_64-pc-windows-msvc.exe (~71 MB)
   ```

   The daemon is `BigTinyV2/daemon`, **not** `plugins/bigtiny_rust`. The V1
   tree is frozen and **nothing builds or links it any more** — Android moved
   to V2 in-process too (D26), so both targets now run one daemon. V1 is kept
   solely as the rollback path; a release built from it would ship an engine
   several phases behind the client code, and would do so silently.

2. **The LiteRT native DLLs ship beside the daemon.** `plugins/build.py`
   stages the **six** DLLs from the LiteRT build output into
   `src-tauri/resources/`, and `bundle.resources` places them in the install
   root next to `bigtiny2-daemon.exe`, where the daemon loads `libLiteRt.dll`
   by bare name and it pulls in the rest:

   - `libLiteRt.dll`
   - `libLiteRtWebGpuAccelerator.dll`
   - `libLiteRtTopKWebGpuSampler.dll`
   - `libwebgpu_dawn.dll`
   - `libGemmaModelConstraintProvider.dll`
   - `litert-lm.dll`

   **Nothing model-shaped ships in the installer.** The EmbeddingGemma model
   and the Gemma `tokenizer.json` it needs are downloaded together, at the
   user's request, from Hugging Face (both are Gemma-licence-gated, so one
   token covers both; the tokenizer comes from `google/embeddinggemma-300m`).
   The summarizer model is an optional download too.

### Signing

Kitty 1.0 ships **unsigned** (a deliberate decision, not an oversight). Before public distribution, obtain an Authenticode certificate and
set Tauri's `bundle.windows.certificateThumbprint` (or `signCommand`) so the
exe + NSIS installer are signed; otherwise SmartScreen warns on first run.

Until then, expect: installing or first-running the unsigned `...-setup.exe`
(or the installed `kitty.exe` itself) shows Windows SmartScreen's "Windows
protected your PC" warning. Users click **More info** → **Run anyway** to
proceed — this is expected, not a build failure.

### Version bump

1. Bump the version everywhere Kitty owns one — `package.json`,
   `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`, and the plugins'
   `Cargo.toml`s. `python scripts/check_versions.py` (run in CI) fails if any
   disagree. The engine (`BigTinyV2/`) keeps its own 2.x versioning; bump it
   only for an engine change, and record a changed API contract in
   `docs/VERSIONS.md`.
2. Re-verify the curated **LiteRT** repos/filenames in
   `src/lib/curated_models.ts` still resolve on HuggingFace: EmbeddingGemma
   `.tflite` (gated — needs an accepted Gemma license + HF token) on both
   platforms, and `gemma-4-E2B-it.litertlm` for the Windows summarizer. A
   renamed repo or a moved license gate turns first run into a dead end.
   Android's `versionCode` follows the version automatically (the Tauri CLI
   derives it when it writes `gen/android/app/tauri.properties`); set
   `bundle.android.versionCode` in `tauri.conf.json` only to override it.
   Play rejects a re-used one.
3. `python plugins/build.py --verify-manifest` passes (rebuild and commit
   whatever it names).
4. If the engine's API surface changed, re-check the route shapes assumed in
   `src-tauri/src/bigtiny/` and `lifecycle/` against `BigTinyV2/API.md`, and
   bump the engine's API version if an older Kitty must refuse it.

### Pre-release verification

- CI green: frontend (lint, vitest, versions), every Rust crate (clippy, test),
  the Windows NSIS and Android AAB bundles, and the binaries-match-source job.
- Secret audit: no secret in any log line, `config.json`, or event payload.
  Provider keys and Kitty's app key live in the Credential Manager; the
  engine's copies of provider keys are sealed under its DPAPI-protected key
  (`%APPDATA%\BigTinyV2\encryption.key.dpapi`, no plaintext `encryption.key`).
- Manual smoke: first-run wizard → chat → a tool approval (in this chat, and
  from another chat while you're elsewhere) → resume a session → export it →
  change a start-up setting and watch the engine restart (or say who is in
  the way).
- Uninstall with "Delete the application data" ticked: the NSIS hook runs
  `kitty.exe --uninstall-cleanup` before asking the engine to stop. Kitty's
  rows are gone from the engine while another app's are intact, the `kitty`
  credentials are gone, and so are the config, models, chat folders and tool
  cache (`%TEMP%\kitty-uninstall.log` lists each step). Unticked, everything
  stays; an upgrade never runs it.
- Soak: repeated summon/dismiss during active streams. Kitty never kills the
  engine — it is shared and exits on its own idle timer.

---

## Bringing a V1 install's data across

A Kitty from before 0.9 ran BigTiny V1, with its data in
`%APPDATA%\Kitty\bigtiny\` (the app-private directory on Android). Kitty now
notices that data and offers to import it (a banner in the main window):
chats and their history, providers, MCP servers and approval rules are merged
into the engine as Kitty's, skipping anything already present; V1's belief
graph comes too if Kitty has none yet. Kitty reads V1's encryption key from the
credential store itself, so saved provider keys are re-sealed under the
engine's key; any it cannot read are counted, and the banner says to enter
those again. The V1 directory is then renamed `bigtiny.v1-imported` (kept as a
rollback copy) and never offered again. "Don't import" is remembered too.

The engine's `bigtiny2-daemon import` CLI still exists for a *fresh* V2
database (it refuses an existing one); Kitty itself no longer needs it.

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

The Gemma `tokenizer.json` is downloaded with the EmbeddingGemma model, as on
Windows; Android has no generative summarizer, so no `.litertlm` and none of
the Windows DLLs ship in the AAB.

### Build

```powershell
pnpm tauri android build --aab --target aarch64    # AAB, release variant
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
`bundle.externalBin`, the **V2** daemon is linked in and hosted in-process
(`lifecycle/bigtiny_embedded.rs`), and the MCP servers register with
`transport: "in_process"`. Android 10+ refuses to `exec()` a binary in
app-writable storage, so a frozen per-plugin executable has nowhere to live.

**The version Gradle stamps comes from Tauri, not from Gradle.**
`gen/android/app/tauri.properties` is gitignored and regenerated from
`tauri.conf.json` on `pnpm tauri android build`. A bare
`gradlew assembleRelease` skips that step and happily ships whatever stale
`versionName`/`versionCode` is on disk — it was two releases behind at 0.9.0/9000
when the Android lane was picked back up. Always go through the Tauri command,
and check the built artifact's version before uploading.

**Delete `app/build/outputs/apk` before every rebuild.** Gradle updates an
existing APK *in place*, and its incremental zip writer appends the new
`lib/arm64-v8a/libkitty_lib.so` without reclaiming the old entry's bytes. The
result is a valid, installable APK carrying a full dead copy of the largest
thing in it — observed twice: 578 MB against 285 MB of live entries (48%
orphaned), and 807 MB on an earlier run. Nothing warns you; the build succeeds
and `adb install` works.

Check any APK you are about to ship or sideload:

```powershell
python -c "import zipfile,os,sys; p=sys.argv[1]; z=zipfile.ZipFile(p); live=sum(i.compress_size for i in z.infolist()); f=os.path.getsize(p); print('%.1f MB file, %.1f MB live, %.1f%% orphaned' % (f/1048576, live/1048576, 100*(f-live)/f))" <path-to.apk>
```

A clean build lands near 0%. Anything above a few percent means the output
directory was reused — delete it and repackage (the Rust is cached, so the
second pass is minutes, not the full compile).

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
- **The app key survives a relaunch.** The host exchanges a per-launch
  registration token for a durable app key on first run and stores it in the
  SecretStore. Force-stop and reopen; the second launch must *not* re-register.
  (A lost key is reclaimed with the registration token, but a relaunch should
  never need to.)
- **The engine's key is sealed.** After first launch there is no plaintext
  `encryption.key` in the app's `bigtiny-v2/` directory, and provider keys
  still work after a force-stop.
- **Specialists and Memorabilia are off.** The model is *not* offered
  `call_specialist`, and neither pane appears in Settings.
- **Share into Kitty** from another app (an image, a PDF, a link): a new chat
  opens with it attached.
- **Notifications**: the four channels (approvals, finished, problems, engine
  status) appear in Android's app settings; tapping an approval notification
  opens that chat with the approval showing.
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
