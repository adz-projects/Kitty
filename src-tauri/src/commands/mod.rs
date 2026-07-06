//! `#[tauri::command]` handlers, grouped into domain submodules. Every command
//! returns `Result<T, String>` with user-safe messages; details are logged
//! with `tracing`, not surfaced to the webview.
//!
//! This used to be one 1141-line file (Stage-1 close-out split it up); each
//! submodule below owns one domain, and this file just re-exports everything
//! so existing `commands::<fn>` call sites (notably `lib.rs`'s
//! `generate_handler!` list) keep working unchanged.

mod config;
mod extensions;
mod file;
mod folders;
mod ollama;
mod provider;
mod session;
mod setup;
mod window;

pub use config::*;
pub use extensions::*;
pub use file::*;
pub use folders::*;
pub use ollama::*;
pub use provider::*;
pub use session::*;
pub use setup::*;
pub use window::*;
