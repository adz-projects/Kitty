//! Build-time fix for the `litert-lm-rust` Windows import library.
//!
//! `litert-lm-rust`'s `download-native` step fetches the prebuilt import lib as
//! `litert-lm.if.lib`, but its build script tells the linker to look for
//! `litert-lm.lib` (`cargo:rustc-link-lib=dylib=litert-lm`). The link therefore
//! fails with `cannot open input file 'litert-lm.lib'`. Until upstream fixes the
//! naming, copy the downloaded `.if.lib` to the expected name in the same
//! (already-searched) directory.
//!
//! Only relevant when the `litert-engine` feature is on and we're on Windows;
//! a no-op otherwise. `litert-lm-rust` is a dependency of this crate, so its
//! build script has already run (and downloaded the lib) by the time this runs.

use std::path::PathBuf;

fn main() {
    // `litert-lm-rust` is a Windows-only, `litert-engine`-gated dependency.
    let engine = std::env::var_os("CARGO_FEATURE_LITERT_ENGINE").is_some();
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if !(engine && windows) {
        return;
    }

    // Our OUT_DIR is `.../build/bigtiny_rust-<hash>/out`; the sibling
    // `.../build/litert-lm-rust-<hash>/out/prebuilt/` holds the downloaded libs.
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(build_dir) = out_dir.ancestors().nth(2).map(PathBuf::from) else {
        return; // .../build
    };

    let Ok(entries) = std::fs::read_dir(&build_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("litert-lm-rust-") {
            continue;
        }
        let prebuilt = entry.path().join("out").join("prebuilt");
        let src = prebuilt.join("litert-lm.if.lib");
        let dst = prebuilt.join("litert-lm.lib");
        if src.exists() && !dst.exists() {
            if let Err(e) = std::fs::copy(&src, &dst) {
                println!("cargo:warning=failed to copy {src:?} -> {dst:?}: {e}");
            } else {
                println!("cargo:warning=litert-lm import lib copied: {dst:?}");
            }
        }
    }
}
