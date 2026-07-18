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
use serde_norway::Value as YamlValue;

fn config_path() -> Result<PathBuf, String> {
    let base = dirs::config_dir().ok_or("could not resolve the user config directory")?;
    Ok(base
        .join("Block")
        .join("goose")
        .join("config")
        .join("config.yaml"))
}

fn read_raw() -> Result<YamlValue, String> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    serde_norway::from_str(&text).map_err(|e| format!("could not parse {}: {e}", path.display()))
}

/// Written atomically (temp file + rename — an atomic replace on Windows),
/// same rationale as Kitty's own `config::save`: this file is shared with
/// other goose processes, so a torn write here would be worse than for
/// Kitty's private config.
fn write_raw(value: &YamlValue) -> Result<(), String> {
    let path = config_path()?;
    let text = serde_norway::to_string(value).map_err(|e| e.to_string())?;
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
    let exts = doc
        .get("extensions")
        .and_then(|v| v.as_mapping())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<ExtensionDefault> = exts
        .iter()
        .filter_map(|(k, v)| {
            let id = k.as_str()?.to_string();
            let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false);
            let ext_type = v
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("builtin")
                .to_string();
            let display_name = v
                .get("display_name")
                .and_then(|d| d.as_str())
                .map(String::from);
            let description = v
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from);
            Some(ExtensionDefault {
                id,
                enabled,
                ext_type,
                display_name,
                description,
            })
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
    let entry = exts
        .get_mut(&key)
        .ok_or_else(|| format!("unknown extension: {id}"))?;
    let map = entry
        .as_mapping_mut()
        .ok_or_else(|| format!("malformed extension entry: {id}"))?;
    map.insert(
        YamlValue::String("enabled".into()),
        YamlValue::Bool(enabled),
    );
    write_raw(&doc)
}

/// Insert/update one literal key in an extension entry's `envs:` mapping.
/// Only for values safe to store in plain text (e.g. a public model tag or
/// URL) — **never a secret**; secrets stay in the OS keyring and go through
/// `env_keys` instead (goose forwards a named var from *its own* process
/// env, it never receives the literal value from this file for those).
///
/// No-op `Ok(())` if the extension isn't registered yet, rather than an
/// error — callers use this as a best-effort sync/migration (e.g. bringing
/// an extension registered before this env var existed up to date), not a
/// required setup step.
pub fn set_extension_env(id: &str, key: &str, value: &str) -> Result<(), String> {
    let mut doc = read_raw()?;
    let exts = doc
        .get_mut("extensions")
        .and_then(|v| v.as_mapping_mut())
        .ok_or("config.yaml has no extensions map")?;
    let id_key = YamlValue::String(id.to_string());
    let Some(entry) = exts.get_mut(&id_key) else {
        return Ok(());
    };
    let map = entry
        .as_mapping_mut()
        .ok_or_else(|| format!("malformed extension entry: {id}"))?;
    set_env_key(map, key, value);
    write_raw(&doc)
}

/// Pure mapping transform behind `set_extension_env`, split out so it's unit
/// testable without touching the filesystem.
fn set_env_key(entry: &mut serde_norway::Mapping, key: &str, value: &str) {
    let envs_key = YamlValue::String("envs".into());
    let is_mapping = entry
        .get(&envs_key)
        .map(|v| v.is_mapping())
        .unwrap_or(false);
    if !is_mapping {
        entry.insert(
            envs_key.clone(),
            YamlValue::Mapping(serde_norway::Mapping::new()),
        );
    }
    let envs_map = entry
        .get_mut(&envs_key)
        .and_then(|v| v.as_mapping_mut())
        .expect("just ensured mapping above");
    envs_map.insert(
        YamlValue::String(key.to_string()),
        YamlValue::String(value.to_string()),
    );
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
    envs: serde_norway::Mapping,
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
        envs: serde_norway::Mapping::new(),
        env_keys,
        timeout: 300,
        cwd: None,
        bundled: None,
    };
    let value = serde_norway::to_value(&entry).map_err(|e| e.to_string())?;
    exts.insert(YamlValue::String(id.to_string()), value);
    write_raw(&doc)
}

/// Idempotently register a bundled internal-plugin stdio extension (see
/// `plugins/README.md`): inserts a fresh, disabled-by-default entry if `id`
/// isn't registered yet, or just refreshes its `cmd`/`args` in place if it
/// already is. Deliberately never touches `enabled` on an existing entry —
/// that's the user's own Settings choice, not something a routine
/// re-registration (called on every app launch, so an update/reinstall's new
/// binary path takes effect) should silently overwrite. Contrast with
/// `add_custom_extension_default`, which always inserts fresh and enabled —
/// that one backs the user-authored "add a custom extension" form, where a
/// second call is a deliberate edit, not a self-heal.
pub fn ensure_extension_registered(id: &str, command: &str, args: &[String]) -> Result<(), String> {
    let mut doc = read_raw()?;
    let exts = doc
        .get_mut("extensions")
        .and_then(|v| v.as_mapping_mut())
        .ok_or("config.yaml has no extensions map")?;
    upsert_bundled_extension_entry(exts, id, command, args)?;
    write_raw(&doc)
}

/// Pure mapping transform behind `ensure_extension_registered`, split out so
/// it's unit testable without touching the filesystem (same pattern as
/// `set_env_key` above).
fn upsert_bundled_extension_entry(
    exts: &mut serde_norway::Mapping,
    id: &str,
    command: &str,
    args: &[String],
) -> Result<(), String> {
    let key = YamlValue::String(id.to_string());
    if let Some(existing) = exts.get_mut(&key) {
        let map = existing
            .as_mapping_mut()
            .ok_or_else(|| format!("malformed extension entry: {id}"))?;
        map.insert(
            YamlValue::String("cmd".into()),
            YamlValue::String(command.to_string()),
        );
        map.insert(
            YamlValue::String("args".into()),
            serde_norway::to_value(args).map_err(|e| e.to_string())?,
        );
    } else {
        let entry = StdioExtensionEntry {
            enabled: false,
            ext_type: "stdio",
            name: id,
            description: "",
            cmd: command,
            args,
            envs: serde_norway::Mapping::new(),
            env_keys: &[],
            timeout: 300,
            cwd: None,
            bundled: Some(true),
        };
        let value = serde_norway::to_value(&entry).map_err(|e| e.to_string())?;
        exts.insert(key, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{set_env_key, upsert_bundled_extension_entry};
    use serde_norway::Value as YamlValue;

    fn entry_with_envs(pairs: &[(&str, &str)]) -> serde_norway::Mapping {
        let mut envs = serde_norway::Mapping::new();
        for (k, v) in pairs {
            envs.insert(
                YamlValue::String(k.to_string()),
                YamlValue::String(v.to_string()),
            );
        }
        let mut entry = serde_norway::Mapping::new();
        entry.insert(YamlValue::String("envs".into()), YamlValue::Mapping(envs));
        entry
    }

    fn get_env<'a>(entry: &'a serde_norway::Mapping, key: &str) -> Option<&'a str> {
        entry
            .get(YamlValue::String("envs".into()))?
            .as_mapping()?
            .get(YamlValue::String(key.into()))?
            .as_str()
    }

    #[test]
    fn set_env_key_creates_envs_map_when_absent() {
        let mut entry = serde_norway::Mapping::new();
        set_env_key(&mut entry, "AP_EMBED_OLLAMA_MODEL", "qwen3-embedding:0.6b");
        assert_eq!(
            get_env(&entry, "AP_EMBED_OLLAMA_MODEL"),
            Some("qwen3-embedding:0.6b")
        );
    }

    #[test]
    fn set_env_key_adds_alongside_existing_keys() {
        let mut entry = entry_with_envs(&[("OTHER_VAR", "keep-me")]);
        set_env_key(&mut entry, "AP_EMBED_OLLAMA_MODEL", "qwen3-embedding:0.6b");
        assert_eq!(get_env(&entry, "OTHER_VAR"), Some("keep-me"));
        assert_eq!(
            get_env(&entry, "AP_EMBED_OLLAMA_MODEL"),
            Some("qwen3-embedding:0.6b")
        );
    }

    #[test]
    fn set_env_key_overwrites_existing_value() {
        let mut entry = entry_with_envs(&[("AP_EMBED_OLLAMA_MODEL", "old-model")]);
        set_env_key(&mut entry, "AP_EMBED_OLLAMA_MODEL", "qwen3-embedding:0.6b");
        assert_eq!(
            get_env(&entry, "AP_EMBED_OLLAMA_MODEL"),
            Some("qwen3-embedding:0.6b")
        );
    }

    #[test]
    fn set_env_key_replaces_non_mapping_envs_value() {
        // Malformed/legacy shape (e.g. `envs: null`) must not panic — just
        // gets replaced with a fresh mapping rather than propagating the bad
        // shape.
        let mut entry = serde_norway::Mapping::new();
        entry.insert(YamlValue::String("envs".into()), YamlValue::Null);
        set_env_key(&mut entry, "AP_EMBED_OLLAMA_MODEL", "qwen3-embedding:0.6b");
        assert_eq!(
            get_env(&entry, "AP_EMBED_OLLAMA_MODEL"),
            Some("qwen3-embedding:0.6b")
        );
    }

    #[test]
    fn upsert_bundled_extension_entry_inserts_disabled_by_default() {
        let mut exts = serde_norway::Mapping::new();
        upsert_bundled_extension_entry(
            &mut exts,
            "replacement-mcp",
            "C:/app/replacement-mcp.exe",
            &[],
        )
        .unwrap();
        let entry = exts
            .get(YamlValue::String("replacement-mcp".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            entry.get(YamlValue::String("enabled".into())),
            Some(&YamlValue::Bool(false))
        );
        assert_eq!(
            entry
                .get(YamlValue::String("cmd".into()))
                .and_then(|v| v.as_str()),
            Some("C:/app/replacement-mcp.exe")
        );
        assert_eq!(
            entry
                .get(YamlValue::String("type".into()))
                .and_then(|v| v.as_str()),
            Some("stdio")
        );
    }

    #[test]
    fn upsert_bundled_extension_entry_updates_cmd_without_touching_enabled() {
        let mut exts = serde_norway::Mapping::new();
        upsert_bundled_extension_entry(
            &mut exts,
            "replacement-mcp",
            "C:/old/replacement-mcp.exe",
            &[],
        )
        .unwrap();
        // Simulate the user having enabled it via Settings.
        {
            let entry = exts
                .get_mut(YamlValue::String("replacement-mcp".into()))
                .unwrap()
                .as_mapping_mut()
                .unwrap();
            entry.insert(YamlValue::String("enabled".into()), YamlValue::Bool(true));
        }

        // A second call (e.g. after a reinstall moved the exe) must update
        // `cmd` in place without silently flipping the user's choice back off.
        upsert_bundled_extension_entry(
            &mut exts,
            "replacement-mcp",
            "C:/new/replacement-mcp.exe",
            &[],
        )
        .unwrap();

        let entry = exts
            .get(YamlValue::String("replacement-mcp".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            entry
                .get(YamlValue::String("cmd".into()))
                .and_then(|v| v.as_str()),
            Some("C:/new/replacement-mcp.exe")
        );
        assert_eq!(
            entry.get(YamlValue::String("enabled".into())),
            Some(&YamlValue::Bool(true))
        );
    }
}
