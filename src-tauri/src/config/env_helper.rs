//! Advanced settings: view/set the user-level Ollama environment variables in
//! `HKCU\Environment` (Phase 5), so users don't hand-edit system env vars.
//! Changes persist for future processes; a running Ollama needs a restart to
//! pick them up.

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

/// The Ollama-relevant variables we expose.
pub const OLLAMA_VARS: &[&str] = &[
    "OLLAMA_HOST",
    "OLLAMA_MODELS",
    "OLLAMA_NUM_PARALLEL",
    "OLLAMA_KEEP_ALIVE",
    "OLLAMA_CONTEXT_LENGTH",
];

/// One env var's current user-level value (`None` if unset).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: Option<String>,
}

/// Read all exposed Ollama env vars from `HKCU\Environment`.
pub fn read_all() -> Vec<EnvVar> {
    let env = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Environment", KEY_READ);
    OLLAMA_VARS
        .iter()
        .map(|name| EnvVar {
            name: (*name).to_string(),
            value: env
                .as_ref()
                .ok()
                .and_then(|e| e.get_value::<String, _>(name).ok()),
        })
        .collect()
}

/// Set (or, when empty/None, clear) one exposed Ollama env var.
pub fn set(name: &str, value: Option<&str>) -> Result<(), String> {
    if !OLLAMA_VARS.contains(&name) {
        return Err(format!("unsupported variable: {name}"));
    }
    let (env, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
        .map_err(|e| format!("registry open failed: {e}"))?;
    match value {
        Some(v) if !v.is_empty() => env
            .set_value(name, &v.to_string())
            .map_err(|e| format!("registry write failed: {e}"))?,
        _ => {
            let _ = env.delete_value(name);
        }
    }
    Ok(())
}
