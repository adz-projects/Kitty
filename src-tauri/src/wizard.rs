//! First-run wizard + repair support (Phase 7): dependency detection, installing
//! missing dependencies via their official installers, autostart, and the
//! setup-completed gate. Installer URLs live in docs/VERSIONS.md.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

use crate::config;
use crate::lifecycle::{goosed, ollama_proc};
use crate::state::AppState;
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

/// Detect Ollama + Goose: presence, version, resolved path, and (best-effort)
/// whether a newer release is available. `goose_override` is the persisted
/// `goose_binary_override` (if the wizard's install or manual-pick already
/// resolved one) — checked before the usual env/bundle/PATH fallbacks.
pub async fn detect(base_url: &str, goose_override: Option<&str>) -> Detection {
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

    let goose_bin = goosed::locate_goose(goose_override);
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
            path: ollama_bin
                .exists()
                .then(|| ollama_bin.display().to_string()),
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

/// Download an installer/archive over HTTPS and either run it (Ollama) or
/// extract it in place and record where (Goose). Only `https` URLs are
/// accepted (no plain http / SSRF surface).
pub async fn install(app: &AppHandle, which: &str) -> Result<(), String> {
    match which {
        "ollama" => install_ollama().await,
        "goose" => install_goose(app).await,
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

/// Exact Windows CLI asset name to look for in the release's asset list —
/// deliberately not `Goose-win32-x64.zip`/`Goose.zip` (the Desktop Electron
/// app; a different, conflicting install — see `docs/VERSIONS.md`) and not
/// the `-cuda` variant (bigger download, GPU-specific, not needed by default).
const GOOSE_CLI_ASSET_NAME: &str = "goose-x86_64-pc-windows-msvc.zip";

/// Goose publishes no Windows `.exe`/`.msi` installer, only plain zip
/// archives — so "install" here means: find the CLI zip's real download URL
/// from the latest GitHub release, download it, extract it into an
/// app-owned folder, and persist that as `goose_binary_override` so
/// `locate_goose` finds it with no further user action.
async fn install_goose(app: &AppHandle) -> Result<(), String> {
    let client = crate::util::http_client();
    let release: serde_json::Value = client
        .get(format!(
            "https://api.github.com/repos/{GOOSE_REPO}/releases/latest"
        ))
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub releases: {e}"))?
        .json()
        .await
        .map_err(|e| format!("could not read release info: {e}"))?;

    let asset_url = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(GOOSE_CLI_ASSET_NAME))
        })
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| {
            format!("could not find {GOOSE_CLI_ASSET_NAME} in the latest Goose release")
        })?
        .to_string();
    if !asset_url.starts_with("https://") {
        return Err("refusing non-HTTPS download URL".into());
    }

    let bytes = client
        .get(&asset_url)
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    let dest_dir = dirs::data_local_dir()
        .ok_or("could not resolve a local app-data directory")?
        .join("Kitty")
        .join("goose");
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("could not create {dest_dir:?}: {e}"))?;

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_ref()))
        .map_err(|e| format!("could not read the downloaded archive: {e}"))?;
    archive
        .extract(&dest_dir)
        .map_err(|e| format!("could not extract the downloaded archive: {e}"))?;

    let goose_exe = find_goose_exe(&dest_dir)
        .ok_or("extracted the Goose archive but couldn't find goose.exe inside it")?;

    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.goose_binary_override = Some(goose_exe.display().to_string());
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Walk a directory tree (a couple levels deep — zip layouts vary) looking
/// for `goose.exe`.
fn find_goose_exe(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("goose.exe") {
            return Some(path);
        }
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    for sub in subdirs {
        if let Some(found) = find_goose_exe(&sub) {
            return Some(found);
        }
    }
    None
}

// --- Adaptive Pathway auto-install (near-term bridge to real packaging) ---
//
// Adaptive Pathway is already on by default (`adaptive_pathway_enabled` in
// `Config`), but that only means Kitty *tries* to spawn its sidecar — if the
// Python package was never installed on the machine, it just reports `Down`
// (existing graceful degradation, see `lifecycle/adaptive_pathway_proc.rs`).
// This is a best-effort, non-blocking attempt to actually install it during
// the wizard, so that graceful-degrade path isn't the common case for a
// brand-new user. This is explicitly a bridge: the real, owner-specified
// target is bundling a standalone sidecar executable as a Tauri
// `externalBin` sidecar (no Python dependency at all) — see the wizard
// redesign plan's Batch 7 for that larger, separate-repo follow-up.

/// Best-effort presence probe: does `adaptive-pathway-sidecar --help` run at
/// all? (Argparse handles `-h`/`--help` after importing the full module tree
/// but before starting the server, so this is a cheap, side-effect-free way
/// to confirm the console script resolves and its imports succeed.)
fn adaptive_pathway_installed() -> bool {
    hidden_command(Path::new("adaptive-pathway-sidecar"))
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Install the Adaptive Pathway sidecar package if it isn't already
/// resolvable. Returns `Ok(true)` if it's installed (already was, or just
/// got installed), `Ok(false)`/`Err` otherwise — callers must treat any
/// non-`Ok(true)` result as non-fatal (see module doc above): the wizard
/// still finishes, and Settings → Advanced keeps a manual retry available.
pub async fn install_adaptive_pathway() -> Result<bool, String> {
    if adaptive_pathway_installed() {
        return Ok(true);
    }
    let output = hidden_command(Path::new("python"))
        .args(["-m", "pip", "install", "adaptive-pathway[sidecar]"])
        .output()
        .map_err(|e| format!("could not run pip (is Python installed?): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pip install failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(adaptive_pathway_installed())
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
