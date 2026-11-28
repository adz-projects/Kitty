//! Sync Android system status and navigation bar icon contrast with Kitty's
//! active light/dark theme (`KittyPlugin.setSystemBarsAppearance`).

use serde::Serialize;

use super::handle;

#[derive(Serialize)]
struct SystemBarsArgs {
    light: bool,
}

/// Update status bar and navigation bar light/dark icon contrast on Android.
/// Best-effort: logs at debug level if the native plugin is unavailable.
pub fn set_appearance(light: bool) {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>(
        "setSystemBarsAppearance",
        SystemBarsArgs { light },
    ) {
        tracing::debug!("system bars appearance update failed: {e}");
    }
}
