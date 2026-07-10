//! Reads/writes goose's own persistent config file — the real source of
//! "which extensions are enabled by default for every new session" (Round-7
//! Feature 4). This is goose's own file, shared with Goose Desktop and any
//! other local goose usage; edits here are a real cross-app surface, not
//! something private to Kitty. Confirmed live (docs/acp-protocol.md): a fresh
//! session's ACP `extensions/list` already reflects exactly whatever's
//! `enabled: true` here — Kitty's session-scoped `extensions/add|remove`
//! calls only ever affected one live session and never touched this file,
//! which is why Settings > Extensions previously edited the active chat
//! instead of the actual default set future sessions start with.
//!
//! Windows path: `%APPDATA%\Block\goose\config\config.yaml`. A change here
//! takes effect on the *next* new session, same as provider/temperature
//! changes needing a `restart_goosed` — not retroactively on any session
//! already open.

use std::path::PathBuf;

use serde::Serialize;
use serde_yaml::Value as YamlValue;

fn config_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir().ok_or("could not resolve the user config directory")?;
    Ok(base.join("Block").join("goose").join("config").join("config.yaml"))
}

fn read_raw() -> Result<YamlValue, String> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    serde_yaml::from_str(&text).map_err(|e| format!("could not parse {}: {e}", path.display()))
}

/// Written atomically (temp file + rename — an atomic replace on Windows),
/// same rationale as Kitty's own `config::save`: this file is shared with
/// other goose processes, so a torn write here would be worse than for
/// Kitty's private config.
fn write_raw(value: &YamlValue) -> Result<(), String> {
    let path = config_path()?;
    let text = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// One entry in the extensions catalog — every extension goose knows about,
/// on or off (unlike the session-scoped ACP `extensions/list`, which only
/// ever shows what's already attached to that one session).
#[derive(Debug, Clone, Serialize)]
pub struct ExtensionDefault {
    pub id: String,
    pub enabled: bool,
    #[serde(rename = "type")]
    pub ext_type: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// The full extensions catalog from goose's own config.yaml.
pub fn list_extension_defaults() -> Result<Vec<ExtensionDefault>, String> {
    let doc = read_raw()?;
    let exts = doc.get("extensions").and_then(|v| v.as_mapping()).cloned().unwrap_or_default();
    let mut out: Vec<ExtensionDefault> = exts
        .iter()
        .filter_map(|(k, v)| {
            let id = k.as_str()?.to_string();
            let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false);
            let ext_type =
                v.get("type").and_then(|t| t.as_str()).unwrap_or("builtin").to_string();
            let display_name =
                v.get("display_name").and_then(|d| d.as_str()).map(String::from);
            let description = v.get("description").and_then(|d| d.as_str()).map(String::from);
            Some(ExtensionDefault { id, enabled, ext_type, display_name, description })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Flip one extension's `enabled` flag, preserving every other key/entry
/// untouched (read as a generic YAML map, not a typed struct, so fields
/// Kitty doesn't know about survive the round trip).
pub fn set_extension_default_enabled(id: &str, enabled: bool) -> Result<(), String> {
    let mut doc = read_raw()?;
    let exts = doc
        .get_mut("extensions")
        .and_then(|v| v.as_mapping_mut())
        .ok_or("config.yaml has no extensions map")?;
    let key = YamlValue::String(id.to_string());
    let entry = exts.get_mut(&key).ok_or_else(|| format!("unknown extension: {id}"))?;
    let map =
        entry.as_mapping_mut().ok_or_else(|| format!("malformed extension entry: {id}"))?;
    map.insert(YamlValue::String("enabled".into()), YamlValue::Bool(enabled));
    write_raw(&doc)
}

/// Add a brand-new custom stdio/MCP extension as a persistent default
/// (Round-3 item 14's form, now persisted here rather than session-scoped
/// only — matches the `stdio`-type shape confirmed in config.yaml, which
/// differs a little from the ACP `mcp`-type shape: `cmd`/`envs`/`env_keys`
/// instead of `server: {command, env}`/`envKeys`).
#[derive(Serialize)]
struct StdioExtensionEntry<'a> {
    enabled: bool,
    #[serde(rename = "type")]
    ext_type: &'static str,
    name: &'a str,
    description: &'a str,
    cmd: &'a str,
    args: &'a [String],
    envs: serde_yaml::Mapping,
    env_keys: &'a [String],
    timeout: u32,
    cwd: Option<String>,
    bundled: Option<bool>,
}

pub fn add_custom_extension_default(
    id: &str,
    command: &str,
    args: &[String],
    env_keys: &[String],
) -> Result<(), String> {
    let mut doc = read_raw()?;
    let exts = doc
        .get_mut("extensions")
        .and_then(|v| v.as_mapping_mut())
        .ok_or("config.yaml has no extensions map")?;
    let entry = StdioExtensionEntry {
        enabled: true,
        ext_type: "stdio",
        name: id,
        description: "",
        cmd: command,
        args,
        envs: serde_yaml::Mapping::new(),
        env_keys,
        timeout: 300,
        cwd: None,
        bundled: None,
    };
    let value = serde_yaml::to_value(&entry).map_err(|e| e.to_string())?;
    exts.insert(YamlValue::String(id.to_string()), value);
    write_raw(&doc)
}
