//! The Android-native surface, reached through one Tauri Android plugin.
//!
//! Two things Rust cannot do on its own here: store a secret somewhere that
//! survives a relaunch (`secrets`), and keep a download running while the app
//! is backgrounded (`download_service`). Both are implemented in Kotlin in
//! `gen/android/app/src/main/java/com/kitty/app/`, and both are reached
//! through the single `PluginHandle` this module owns.
//!
//! The Kotlin lives in the *app* module rather than a separate Gradle library
//! because it is one app's glue, not a reusable plugin. Tauri resolves the
//! class through the activity's own classloader (`find_class`), so the app
//! module works exactly as a library module would.
//!
//! Nothing here is exposed to the webview: `capabilities/default.json` grants
//! none of these commands, so no JS — including anything a model talks a tool
//! into emitting — can read a stored API key.

pub mod download_service;
pub mod logcat;
pub mod secrets;

use std::sync::OnceLock;

use tauri::plugin::{PluginHandle, TauriPlugin};
use tauri::Wry;

/// Package and class of the Kotlin side. `KittyPlugin` is annotated
/// `@TauriPlugin`, which is what keeps its `@Command` methods from being
/// stripped by R8 in the minified release build (the keep rule ships in
/// `:tauri-android`'s consumer proguard file).
const PLUGIN_IDENTIFIER: &str = "com.kitty.app";
const PLUGIN_CLASS: &str = "KittyPlugin";

/// Set once during `setup`.
///
/// Concretely `Wry`, and this module is deliberately not generic over the
/// runtime: the callers are plain free functions
/// (`config::providers::keyring::get_secret` and friends, reached from
/// everywhere including non-Tauri contexts), and making them all generic to
/// thread a type parameter that only ever has one value would be a large,
/// load-bearing-looking change for no expressiveness.
static HANDLE: OnceLock<PluginHandle<Wry>> = OnceLock::new();

/// Register the Kotlin plugin. Must be added to the builder before anything
/// tries to read a secret — in practice, before `start_stack`, since provider
/// registration reads keys.
pub fn init() -> TauriPlugin<Wry> {
    tauri::plugin::Builder::new("kitty-native")
        .setup(|_app, api| {
            let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, PLUGIN_CLASS)?;
            if HANDLE.set(handle).is_err() {
                tracing::warn!("the Android native plugin was registered twice");
            }
            Ok(())
        })
        .build()
}

/// The registered handle, or an error before `setup` has run.
///
/// Reaching the error means something ran before `setup`, which is a wiring
/// bug rather than a runtime condition — but it is reported rather than
/// panicked on, because the alternative is taking the whole app down over a
/// secret read.
pub(crate) fn handle() -> Result<&'static PluginHandle<Wry>, String> {
    HANDLE
        .get()
        .ok_or_else(|| "the Android native plugin is not registered yet".to_string())
}
