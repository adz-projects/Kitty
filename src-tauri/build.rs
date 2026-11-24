fn main() {
    bundled_engine_version();
    tauri_build::build()
}

/// The engine version this build bundles (`BigTinyV2/daemon/Cargo.toml`), as
/// `KITTY_BUNDLED_ENGINE`: an engine already running at start-up that is older
/// is asked to restart, so an upgrade takes effect
/// (`lifecycle::engine_restart::check_engine_version`).
fn bundled_engine_version() {
    let manifest = "../BigTinyV2/daemon/Cargo.toml";
    println!("cargo:rerun-if-changed={manifest}");
    let text = std::fs::read_to_string(manifest).unwrap_or_default();
    let mut in_package = false;
    let mut version = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("version") {
                let rest = rest.trim_start();
                if let Some(v) = rest.strip_prefix('=') {
                    version = v.trim().trim_matches('"').to_string();
                    break;
                }
            }
        }
    }
    println!("cargo:rustc-env=KITTY_BUNDLED_ENGINE={version}");
}
