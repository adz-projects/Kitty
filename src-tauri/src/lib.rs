//! Goose Overlay — Tauri v2 application entry point.
//!
//! One Rust process owns four labelled windows, the goosed + Ollama process
//! lifecycle, config, tray, and the global hotkey. All I/O lives here; the
//! webview only talks to us through the commands registered below.

mod commands;
mod config;
mod goosed;
mod hotkey;
mod lifecycle;
mod state;
mod tray;
mod util;
mod windows;

use tauri::RunEvent;

use state::AppState;

pub fn run() {
    // Structured logs to stderr; RUST_LOG overrides the default filter.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "goose_overlay_lib=info,warn".into()),
        )
        .try_init();

    let cfg = config::load().unwrap_or_else(|e| {
        tracing::warn!("config load failed ({e}); using defaults");
        config::Config::default()
    });
    let hotkey_accel = cfg.hotkey.clone();

    let mut builder = tauri::Builder::default();

    // Single-instance MUST be the first plugin registered (Tauri guidance).
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch summons the first instance's overlay.
            if let Err(e) = windows::show_overlay(app) {
                tracing::warn!("single-instance show_overlay failed: {e}");
            }
        }));
    }

    builder
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new(cfg))
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::toggle_overlay,
            commands::hide_overlay,
            commands::open_settings,
            commands::open_main,
            commands::get_stack_status,
            commands::restart_goosed,
            commands::new_session,
            commands::send_prompt,
            commands::set_active_session,
            commands::get_active_session,
        ])
        .setup(move |app| {
            let handle = app.handle();
            windows::create_overlay(handle)?;
            tray::create(handle)?;
            if let Err(e) = hotkey::register(handle, &hotkey_accel) {
                tracing::error!("global hotkey registration failed: {e}");
            }
            // Start Ollama + goosed and the health loop in the background.
            lifecycle::start_stack(handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Goose Overlay application")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // Kill only the children we spawned.
                lifecycle::shutdown(app);
            }
        });
}
