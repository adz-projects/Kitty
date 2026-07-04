//! First-run wizard + repair support (Phase 7): dependency detection, installing
//! missing dependencies via their official installers, autostart, and the
//! setup-completed gate. Installer URLs live in docs/VERSIONS.md.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

use crate::lifecycle::{goosed, ollama_proc};
use crate::util::hidden_command;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "GooseOverlay";

/// Official installer download URLs (Windows). Verify on version bumps.
const OLLAMA_INSTALLER_URL: &str = "https://ollama.com/download/OllamaSetup.exe";

#[derive(Debug, Clone, Serialize)]
pub struct DepStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub ollama: DepStatus,
    pub goose: DepStatus,
}

fn run_version(bin: &Path) -> Option<String> {
    let out = hidden_command(bin).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let combined = if text.trim().is_empty() {
        String::from_utf8_lossy(&out.stderr).to_string()
    } else {
        text.to_string()
    };
    let v = combined.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Detect Ollama + Goose: presence, version, and resolved path.
pub async fn detect(base_url: &str) -> Detection {
    // Ollama: prefer the running server's version, else the binary.
    let ollama_bin = ollama_proc::locate_ollama();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok();
    let ollama_ver = match client {
        Some(c) => {
            let url = format!("{}/api/version", base_url.trim_end_matches('/'));
            match c.get(url).send().await {
                Ok(r) => r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|j| j.get("version").and_then(|v| v.as_str()).map(String::from)),
                Err(_) => None,
            }
        }
        None => None,
    };
    let ollama_installed = ollama_bin.exists() || ollama_ver.is_some();

    let goose_bin = goosed::locate_goose();
    let goose_installed = goose_bin.exists();
    let goose_ver = goose_installed.then(|| run_version(&goose_bin)).flatten();

    Detection {
        ollama: DepStatus {
            installed: ollama_installed,
            version: ollama_ver,
            path: ollama_bin.exists().then(|| ollama_bin.display().to_string()),
        },
        goose: DepStatus {
            installed: goose_installed,
            version: goose_ver,
            path: goose_installed.then(|| goose_bin.display().to_string()),
        },
    }
}

/// Download an installer over HTTPS and run it, returning once it exits. Only
/// `https` URLs are accepted (no plain http / SSRF surface).
pub async fn install(which: &str) -> Result<(), String> {
    let url = match which {
        "ollama" => OLLAMA_INSTALLER_URL,
        // Goose ships a Windows installer via Block's GitHub releases; the exact
        // asset URL is pinned in docs/VERSIONS.md once verified.
        "goose" => return Err("Automatic Goose install isn't wired yet — install Goose Desktop from block/goose releases, then re-detect.".into()),
        _ => return Err("unknown dependency".into()),
    };
    if !url.starts_with("https://") {
        return Err("refusing non-HTTPS installer URL".into());
    }

    let bytes = reqwest::get(url)
        .await
        .map_err(|e| format!("download failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    let mut path = std::env::temp_dir();
    path.push(format!("{which}-setup.exe"));
    std::fs::write(&path, &bytes).map_err(|e| format!("could not save installer: {e}"))?;

    // Hand off to the installer's own UI (its UAC prompt handles elevation).
    hidden_command(&path)
        .spawn()
        .map_err(|e| format!("could not launch installer: {e}"))?;
    Ok(())
}

// --- Autostart (HKCU Run key) ---

pub fn autostart_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
        .map_err(|e| e.to_string())?;
    if enabled {
        let exe: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))
            .map_err(|e| e.to_string())?;
    } else {
        let _ = key.delete_value(RUN_VALUE);
    }
    Ok(())
}

/// True if first-run setup is complete (drives wizard-vs-overlay on launch).
pub fn setup_completed(app: &AppHandle) -> bool {
    app.state::<crate::state::AppState>()
        .config
        .lock()
        .unwrap()
        .setup_completed
}
