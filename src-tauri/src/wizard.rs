//! First-run wizard + repair support: dependency detection, installing
//! missing dependencies via their official installers, autostart, and the
//! setup-completed gate. Installer URLs live in docs/VERSIONS.md.

use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

use crate::lifecycle::ollama_proc;
use crate::util::hidden_command;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Kitty";
/// Pre-rename value name. Windows shows the value name verbatim in Task
/// Manager → Startup and in Settings → Apps → Startup, so an install that
/// enabled autostart before the Goose Overlay → Kitty rename lists itself
/// under the old product's name. Read as a fallback and cleaned up on the
/// next write (see `autostart_enabled`/`set_autostart`).
const OLD_RUN_VALUE: &str = "GooseOverlay";

/// Official installer download URLs (Windows). Verify on version bumps.
const OLLAMA_INSTALLER_URL: &str = "https://ollama.com/download/OllamaSetup.exe";

#[derive(Debug, Clone, Serialize)]
pub struct DepStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
    /// Latest released version, if the GitHub Releases check succeeded
    /// (Round-3 item 29). `None` on any lookup failure — never blocks detection.
    pub latest_version: Option<String>,
    /// `Some(true)` when `version` is a parseable semver strictly older than
    /// `latest_version`; `None` if either side didn't parse or wasn't found.
    pub is_outdated: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub ollama: DepStatus,
}

/// GitHub repo whose Releases API we check for the latest version (item 29).
const OLLAMA_REPO: &str = "ollama/ollama";

/// Fetch a repo's latest release tag via the GitHub Releases API. GitHub
/// requires a `User-Agent`; failures (offline, rate-limited, etc.) return
/// `None` and never block the rest of detection.
async fn latest_github_release(repo: &str) -> Option<String> {
    let client = crate::util::http_client();
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let json: serde_json::Value = client
        .get(url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    json.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches('v').to_string())
}

/// Find the first semver-shaped token in free-form text (CLI `--version`
/// output and GitHub tags both tend to be "mostly semver with noise around
/// it" rather than guaranteed-clean). Loosely pads 2-component versions
/// (`0.31` → `0.31.0`) since Ollama/Goose version strings aren't always
/// strictly 3-component.
fn find_semver(text: &str) -> Option<semver::Version> {
    for word in text.split_whitespace() {
        let w = word
            .trim_start_matches(['v', 'V'])
            .trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if let Ok(v) = semver::Version::parse(w) {
            return Some(v);
        }
        let core: String = w
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if core.is_empty() {
            continue;
        }
        if let Ok(v) = semver::Version::parse(&core) {
            return Some(v);
        }
        if core.split('.').count() == 2 {
            if let Ok(v) = semver::Version::parse(&format!("{core}.0")) {
                return Some(v);
            }
        }
    }
    None
}

/// `Some(true)` iff both sides parse as semver and `installed < latest`.
fn is_outdated(installed: Option<&str>, latest: Option<&str>) -> Option<bool> {
    let cur = find_semver(installed?)?;
    let lat = find_semver(latest?)?;
    Some(cur < lat)
}

/// Detect Ollama: presence, version, resolved path, and (best-effort)
/// whether a newer release is available.
pub async fn detect(base_url: &str) -> Detection {
    // Ollama: prefer the running server's version, else the binary.
    let ollama_bin = ollama_proc::locate_ollama();
    let url = format!("{}/api/version", base_url.trim_end_matches('/'));
    let ollama_ver = match crate::util::http_client()
        .get(url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|j| j.get("version").and_then(|v| v.as_str()).map(String::from)),
        Err(_) => None,
    };
    let ollama_installed = ollama_bin.exists() || ollama_ver.is_some();

    let ollama_latest = latest_github_release(OLLAMA_REPO).await;

    Detection {
        ollama: DepStatus {
            installed: ollama_installed,
            is_outdated: is_outdated(ollama_ver.as_deref(), ollama_latest.as_deref()),
            version: ollama_ver,
            path: ollama_bin
                .exists()
                .then(|| ollama_bin.display().to_string()),
            latest_version: ollama_latest,
        },
    }
}

/// Download an installer over HTTPS and run it. Only `https` URLs are
/// accepted (no plain http / SSRF surface).
pub async fn install(_app: &AppHandle, which: &str) -> Result<(), String> {
    match which {
        "ollama" => install_ollama().await,
        _ => Err("unknown dependency".into()),
    }
}

async fn install_ollama() -> Result<(), String> {
    if !OLLAMA_INSTALLER_URL.starts_with("https://") {
        return Err("refusing non-HTTPS installer URL".into());
    }

    let bytes = crate::util::http_client()
        .get(OLLAMA_INSTALLER_URL)
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    let mut path = std::env::temp_dir();
    path.push("ollama-setup.exe");
    std::fs::write(&path, &bytes).map_err(|e| format!("could not save installer: {e}"))?;

    // Hand off to the installer's own UI (its UAC prompt handles elevation).
    hidden_command(&path)
        .spawn()
        .map_err(|e| format!("could not launch installer: {e}"))?;
    Ok(())
}

// --- Autostart (HKCU Run key) ---

/// True if either the current or the pre-rename value is present, so an
/// install that enabled autostart before the rename still reads as enabled
/// instead of silently appearing off (and then getting a duplicate entry
/// written under the new name).
pub fn autostart_enabled() -> bool {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ)
    else {
        return false;
    };
    key.get_value::<String, _>(RUN_VALUE).is_ok()
        || key.get_value::<String, _>(OLD_RUN_VALUE).is_ok()
}

/// Writes (or clears) the HKCU Run entry. Always removes the pre-rename
/// value too, so enabling migrates an old entry rather than leaving both
/// listed in Task Manager → Startup, and disabling can't leave a stale one
/// behind that keeps launching the app.
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
        .map_err(|e| e.to_string())?;
    let _ = key.delete_value(OLD_RUN_VALUE);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_outdated_detects_older_installed_version() {
        assert_eq!(is_outdated(Some("0.31.1"), Some("0.32.0")), Some(true));
        assert_eq!(
            is_outdated(Some("ollama version is 1.41.0"), Some("v1.41.0")),
            Some(false)
        );
        assert_eq!(is_outdated(Some("0.31"), Some("0.31.0")), Some(false));
        assert_eq!(is_outdated(None, Some("1.0.0")), None);
        assert_eq!(is_outdated(Some("not a version"), Some("1.0.0")), None);
    }
}
