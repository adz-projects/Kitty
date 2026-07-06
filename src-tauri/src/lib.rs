//! Goose Overlay — Tauri v2 application entry point.
//!
//! One Rust process owns four labelled windows, the goosed + Ollama process
//! lifecycle, config, tray, and the global hotkey. All I/O lives here; the
//! webview only talks to us through the commands registered below.

mod commands;
mod config;
#[cfg(windows)]
mod copilot;
mod goosed;
mod hotkey;
mod lifecycle;
mod notifications;
mod ollama;
mod state;
mod tray;
mod util;
mod windows;
mod wizard;

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
    let hotkeys = cfg.hotkeys.clone();
    let clipboard_hotkey = cfg.clipboard_hotkey.clone();

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
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
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
            commands::cancel_prompt,
            commands::set_active_session,
            commands::get_active_session,
            commands::respond_permission,
            commands::set_mode,
            commands::list_sessions,
            commands::load_session,
            commands::delete_session,
            commands::fork_session,
            commands::read_text_file,
            commands::read_file_any,
            commands::write_file,
            commands::list_folders,
            commands::create_folder,
            commands::rename_folder,
            commands::delete_folder,
            commands::assign_session_folder,
            commands::get_session_mode,
            commands::set_session_mode,
            commands::inspect_paths,
            commands::open_path,
            commands::reveal_path,
            commands::list_providers,
            commands::upsert_provider,
            commands::delete_provider,
            commands::activate_provider,
            commands::ollama_list_models,
            commands::ollama_delete_model,
            commands::ollama_pull_model,
            commands::read_ollama_env,
            commands::set_ollama_env,
            commands::restart_ollama,
            commands::list_extensions,
            commands::set_extension_enabled,
            commands::add_extension,
            commands::get_settings_target,
            commands::list_themes,
            commands::read_user_theme,
            commands::open_themes_folder,
            commands::read_image_data_url,
            commands::detect_dependencies,
            commands::install_dependency,
            commands::open_wizard,
            commands::get_wizard_mode,
            commands::complete_setup,
            commands::get_autostart,
            commands::set_autostart,
        ])
        .setup(move |app| {
            let handle = app.handle();
            windows::create_overlay(handle)?;
            tray::create(handle)?;
            if let Err(e) = hotkey::register(handle, &hotkeys, clipboard_hotkey.as_deref()) {
                tracing::error!("global hotkey registration failed: {e}");
            }
            // Low-level Copilot-key hook (Windows only).
            #[cfg(windows)]
            copilot::install(handle);
            // First launch: show the setup wizard instead of the (hidden) overlay.
            if !wizard::setup_completed(handle) {
                let _ = windows::open_wizard(handle, "setup");
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
