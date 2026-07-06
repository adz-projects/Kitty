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

/// GitHub repos whose Releases API we check for the latest version (item 29).
/// Goose's org was renamed from `block` to `aaif-goose` after this was first
/// pinned (Stage-1 close-out) — GitHub redirects the old path, but point at
/// the canonical one rather than lean on that indefinitely.
const OLLAMA_REPO: &str = "ollama/ollama";
const GOOSE_REPO: &str = "aaif-goose/goose";

/// Fetch a repo's latest release tag via the GitHub Releases API. GitHub
/// requires a `User-Agent`; failures (offline, rate-limited, etc.) return
/// `None` and never block the rest of detection.
async fn latest_github_release(repo: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("kitty-app")
        .build()
        .ok()?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let json: serde_json::Value = client.get(url).send().await.ok()?.json().await.ok()?;
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
        let core: String = w.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
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

/// Detect Ollama + Goose: presence, version, resolved path, and (best-effort)
/// whether a newer release is available.
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

    let (ollama_latest, goose_latest) = tokio::join!(
        latest_github_release(OLLAMA_REPO),
        latest_github_release(GOOSE_REPO)
    );

    Detection {
        ollama: DepStatus {
            installed: ollama_installed,
            is_outdated: is_outdated(ollama_ver.as_deref(), ollama_latest.as_deref()),
            version: ollama_ver,
            path: ollama_bin.exists().then(|| ollama_bin.display().to_string()),
            latest_version: ollama_latest,
        },
        goose: DepStatus {
            installed: goose_installed,
            is_outdated: is_outdated(goose_ver.as_deref(), goose_latest.as_deref()),
            version: goose_ver,
            path: goose_installed.then(|| goose_bin.display().to_string()),
            latest_version: goose_latest,
        },
    }
}

/// Download an installer over HTTPS and run it, returning once it exits. Only
/// `https` URLs are accepted (no plain http / SSRF surface).
pub async fn install(which: &str) -> Result<(), String> {
    let url = match which {
        "ollama" => OLLAMA_INSTALLER_URL,
        // Confirmed (Stage-1 close-out): Goose publishes no Windows .exe/.msi
        // installer at all, only plain zip archives (see docs/VERSIONS.md) — so
        // there's nothing here to silently download-and-run like Ollama's. The
        // wizard UI no longer offers an "Install" button for Goose at all (it
        // links straight to the release page instead); this arm only guards a
        // direct/future call into this command.
        "goose" => {
            return Err(
                "Goose has no automatic installer on Windows — download \
                 goose-x86_64-pc-windows-msvc.zip from its GitHub releases and extract it, \
                 then re-check."
                    .into(),
            )
        }
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
