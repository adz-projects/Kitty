//! `kitty.exe --uninstall-cleanup` (#87): what the uninstaller runs when the
//! user ticks "also delete my data".
//!
//! Handled at the very top of `run()`, before Tauri builds anything, so no
//! window, tray or hotkey appears. Everything is best-effort: an uninstall
//! must not be blocked by one thing that could not be removed, so each step
//! is logged (to `%TEMP%\kitty-uninstall.log`) and the process exits 0.
//!
//! Kitty's data in the shared engine goes through the engine
//! (`DELETE /api/apps/me?purge=true`), which removes only rows Kitty owns;
//! other apps attached to the same engine keep theirs. The uninstaller runs
//! this *before* asking the engine to stop, because reaching it may start it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const FLAG: &str = "--uninstall-cleanup";

pub fn requested() -> bool {
    std::env::args().skip(1).any(|a| a == FLAG)
}

pub fn run() -> i32 {
    let mut log = Log::open();
    let cfg = crate::config::load().unwrap_or_else(|e| {
        log.line(&format!("config unreadable ({e}); using defaults"));
        Default::default()
    });

    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => match rt.block_on(purge_engine_data(&cfg)) {
            Ok(()) => log.line("engine: Kitty's data removed"),
            Err(e) => log.line(&format!("engine: {e}")),
        },
        Err(e) => log.line(&format!("engine: no async runtime ({e})")),
    }

    for account in secret_accounts(&cfg) {
        crate::config::providers::delete_secret(&account);
    }
    log.line("credentials: removed");

    for dir in dirs_to_remove(&cfg) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => log.line(&format!("removed {}", dir.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log.line(&format!("could not remove {}: {e}", dir.display())),
        }
        // `Documents\Kitty` once its `chats` is gone, if nothing else is in it.
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
    0
}

/// Reach the engine as Kitty and have it remove everything Kitty owns.
async fn purge_engine_data(cfg: &crate::config::Config) -> Result<(), String> {
    let snap = crate::lifecycle::bigtiny_env::SpawnSnapshot::from_config(cfg);
    let lib_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("resources")))
        .map(|d| d.to_string_lossy().into_owned());
    let handle = crate::lifecycle::bigtiny_v2::locate(&snap, "", lib_dir.as_deref()).await?;
    let port = handle.port.ok_or("the engine did not report a port")?;
    let client = crate::bigtiny::client::BigTinyClient::new(
        format!("http://127.0.0.1:{port}"),
        handle.secret_key,
    );
    client.delete("/api/apps/me?purge=true").await.map(|_| ())
}

/// Every credential Kitty stores: one per provider, plus its fixed ones.
fn secret_accounts(cfg: &crate::config::Config) -> Vec<String> {
    let mut accounts: Vec<String> = cfg.providers.iter().map(|p| p.id.clone()).collect();
    accounts.extend(
        [
            crate::lifecycle::bigtiny_app_key::KEY_CREDENTIAL,
            crate::config::providers::V1_ENCRYPTION_KEY_ACCOUNT,
            "brave-mcp-search",
        ]
        .map(String::from),
    );
    accounts
}

/// Kitty's settings, downloaded models, chat folders (current and earlier
/// locations) and the tool cache.
fn dirs_to_remove(cfg: &crate::config::Config) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = crate::config::config_dir() {
        dirs.push(dir);
    }
    if let Ok(dir) = crate::config::models_dir() {
        dirs.push(dir);
    }
    dirs.extend(crate::commands::chats_roots_for(cfg));
    if let Some(home) = dirs::home_dir() {
        dirs.push(tool_cache(&home));
    }
    dirs
}

/// The bundled tools' cache (see `kitty-tools`' `paths.rs`).
fn tool_cache(home: &Path) -> PathBuf {
    home.join(".cache").join("lean-goose-mcp")
}

struct Log(Option<std::fs::File>);

impl Log {
    fn open() -> Self {
        let path = std::env::temp_dir().join("kitty-uninstall.log");
        Self(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok(),
        )
    }

    fn line(&mut self, text: &str) {
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "{} {text}", chrono::Utc::now().to_rfc3339());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_key_and_fixed_credential_is_removed() {
        let mut cfg = crate::config::Config::default();
        let p: crate::config::providers::ProviderProfile = serde_json::from_value(
            serde_json::json!({ "id": "prov-1", "name": "P", "provider_type": "openai", "base_url": "", "created_at": "" }),
        )
        .unwrap();
        cfg.providers.push(p);
        let accounts = secret_accounts(&cfg);
        assert!(accounts.contains(&"prov-1".to_string()));
        assert!(accounts.contains(&"bigtiny-v2-app-key".to_string()));
        assert!(accounts.contains(&"bigtiny-encryption-key".to_string()));
    }

    #[test]
    fn chat_folders_are_removed_not_the_folder_around_them() {
        let cfg = crate::config::Config {
            default_context_folder: Some("D:/Docs/Kitty".into()),
            chats_roots_history: vec!["E:/Old".into()],
            ..Default::default()
        };
        let dirs = dirs_to_remove(&cfg);
        assert!(dirs.contains(&PathBuf::from("D:/Docs/Kitty").join("chats")));
        assert!(dirs.contains(&PathBuf::from("E:/Old").join("chats")));
        assert!(!dirs.contains(&PathBuf::from("D:/Docs/Kitty")));
    }
}
