//! `#[tauri::command]` handlers, grouped into domain submodules. Every command
//! returns `Result<T, String>` with user-safe messages; details are logged
//! with `tracing`, not surfaced to the webview.
//!
//! This used to be one 1141-line file (Stage-1 close-out split it up); each
//! submodule below owns one domain, and this file just re-exports everything
//! so existing `commands::<fn>` call sites (notably `lib.rs`'s
//! `generate_handler!` list) keep working unchanged.

mod adaptive_pathway;
mod config;
mod file;
mod folders;
mod logs;
mod mcp_servers;
mod memory;
mod models;
mod provider;
mod recipes;
mod scheduled_tasks;
// Win32 GDI desktop capture — see `crate::screenshot` (docs/ANDROID.md §2.5).
#[cfg(windows)]
mod screenshot;
mod session;
mod setup;
mod window;

pub use adaptive_pathway::*;
pub use config::*;
pub use file::*;
pub use folders::*;
pub use logs::*;
pub use mcp_servers::*;
pub use memory::*;
pub use models::*;
pub use provider::*;
pub use recipes::*;
pub use scheduled_tasks::*;
#[cfg(windows)]
pub use screenshot::*;
pub use session::*;
pub use setup::*;
pub use window::*;
