//! Managed application state: window-agnostic runtime data shared across commands.
//!
//! Holds the loaded config plus the process/health machinery populated in Phase 1
//! (goosed + Ollama handles, generated secret/port, current stack status).

use std::sync::Mutex;

use crate::config::Config;
use crate::lifecycle::{ManagedProcess, StackStatus};

/// Root managed state, registered via `app.manage(AppState::new(..))`.
pub struct AppState {
    /// App configuration (persisted to `%APPDATA%/goose-overlay/config.json`).
    pub config: Mutex<Config>,
    /// The goosed (`goose serve`) child we spawn, plus its port + secret key.
    pub goosed: Mutex<GoosedHandle>,
    /// The Ollama process — only `Some(child)` when *we* started it.
    pub ollama: Mutex<ManagedProcess>,
    /// Last computed stack status, so the health loop only emits on change.
    pub stack_status: Mutex<StackStatus>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            goosed: Mutex::new(GoosedHandle::default()),
            ollama: Mutex::new(ManagedProcess::default()),
            stack_status: Mutex::new(StackStatus::default()),
        }
    }
}

/// Connection details for the goosed ACP server we manage.
#[derive(Default)]
pub struct GoosedHandle {
    pub process: ManagedProcess,
    pub port: Option<u16>,
    /// Sent as `X-Secret-Key` on ACP requests — consumed by `goosed/api.rs` in Phase 2.
    #[allow(dead_code)]
    pub secret_key: Option<String>,
}

impl GoosedHandle {
    /// Base URL of the local ACP endpoint, if the server has been started.
    /// Used by the ACP client in Phase 2.
    #[allow(dead_code)]
    pub fn base_url(&self) -> Option<String> {
        self.port.map(|p| format!("http://127.0.0.1:{p}"))
    }
}
