//! Kitty — Tauri v2 application entry point.
//!
//! One Rust process owns four labelled windows, the BigTiny + Ollama process
//! lifecycle, config, tray, and the global hotkey. All I/O lives here; the
//! webview only talks to us through the commands registered below.

mod bigtiny;
mod commands;
mod config;
// Global hotkeys and the tray both exist only on desktop: Android has no
// OS-wide shortcut registration and `tauri::tray` is itself gated on
// `all(desktop, feature = "tray-icon")`. See docs/ANDROID.md D23/§2.5.
#[cfg(desktop)]
mod hotkey;
mod lifecycle;
mod log_capture;
mod models;
mod notifications;
mod openrouter;
// Raw Win32 GDI desktop capture (BitBlt/GetDIBits) — the `windows` crate is a
// `cfg(windows)`-only dependency, so this module cannot compile elsewhere.
#[cfg(windows)]
mod screenshot;
mod state;
#[cfg(desktop)]
mod tray;
mod util;
mod windows;
mod wizard;

use tauri::RunEvent;
use tracing_subscriber::prelude::*;

use state::AppState;

pub fn run() {
    // Structured logs to stderr (unchanged); RUST_LOG overrides the default
    // filter. Also captures WARN/ERROR events into an in-memory ring buffer
    // (`log_capture`) that Settings → Advanced's error log reads — same
    // filter applies to both layers via `.with(env_filter)` on the registry.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "kitty_lib=info,warn".into());
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(log_capture::CaptureLayer)
        .with(env_filter)
        .try_init();

    let (cfg, config_recovered) = config::load_with_recovery();
    // One-time migration from the pre-rename `goose-overlay` keyring service
    // (config.json itself already migrated by `config::load` -> `config_dir`).
    let provider_ids: Vec<String> = cfg.providers.iter().map(|p| p.id.clone()).collect();
    config::providers::migrate_secrets(&provider_ids);
    // Only read on desktop — `hotkey::register` is the sole consumer, and the
    // config fields themselves stay in the struct on every platform so a
    // config.json round-trips unchanged between them.
    #[cfg(desktop)]
    let hotkeys = cfg.hotkeys.clone();
    #[cfg(desktop)]
    let clipboard_hotkey = cfg.clipboard_hotkey.clone();
    #[cfg(desktop)]
    let open_window_hotkey = cfg.open_window_hotkey.clone();

    // `mut` is only needed by the desktop-only plugin blocks below.
    #[cfg_attr(not(desktop), allow(unused_mut))]
    let mut builder = tauri::Builder::default();

    // Single-instance MUST be the first plugin registered (Tauri guidance).
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch attempt (taskbar-pinned icon, Start-menu
            // shortcut, or double-clicking the exe again while already
            // running) focuses/opens a chat window in the first instance —
            // never the overlay (see `focus_or_open_chat_window`'s doc
            // comment). The hotkey remains the overlay's own summon path.
            windows::focus_or_open_chat_window(app);
        }));
    }

    // Lifted out of the chain below rather than attributed inline: a `#[cfg]`
    // on a single `.plugin(..)` call in a method chain isn't valid, so this
    // follows the same re-assignment shape as single-instance above.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());
    }

    builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::new(
            cfg,
            config_recovered.map(|p| p.to_string_lossy().into_owned()),
        ))
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::get_config_recovery_notice,
            commands::toggle_overlay,
            commands::hide_overlay,
            commands::open_settings,
            commands::open_main,
            commands::open_new_chat_window,
            #[cfg(windows)]
            commands::capture_screenshot_region,
            #[cfg(windows)]
            commands::get_screenshot_preview,
            #[cfg(windows)]
            commands::report_screenshot_selection,
            #[cfg(windows)]
            commands::cancel_screenshot_selection,
            commands::get_pending_handoff,
            commands::get_stack_status,
            commands::get_engine_restart_state,
            commands::get_startup_phase,
            commands::window_ready,
            commands::restart_backend,
            commands::new_session,
            commands::bind_window_session,
            commands::send_prompt,
            commands::cancel_prompt,
            commands::is_session_busy,
            commands::set_active_session,
            commands::get_active_session,
            commands::respond_permission,
            commands::notify_approval_needed,
            commands::set_mode,
            commands::list_sessions,
            commands::load_session,
            commands::delete_session,
            commands::rename_session,
            commands::clear_all_sessions,
            commands::fork_session,
            commands::compact_session,
            commands::set_thinking_effort,
            commands::rebind_session_provider,
            commands::read_text_file,
            commands::read_file_any,
            commands::copy_file_into_chat_folder,
            commands::write_file,
            commands::list_folders,
            commands::create_folder,
            commands::rename_folder,
            commands::delete_folder,
            commands::assign_session_folder,
            commands::list_scheduled_tasks,
            commands::create_scheduled_task,
            commands::update_scheduled_task,
            commands::delete_scheduled_task,
            commands::set_scheduled_task_enabled,
            commands::list_recipes,
            commands::create_recipe,
            commands::update_recipe,
            commands::delete_recipe,
            commands::duplicate_recipe,
            commands::import_recipe_yaml,
            commands::export_recipe_yaml,
            commands::add_recipe_extension,
            commands::list_log_entries,
            commands::clear_log_entries,
            commands::get_memory_stats,
            commands::get_session_mode,
            commands::set_session_mode,
            commands::set_session_context_dir,
            commands::set_session_persona_override,
            commands::inspect_paths,
            commands::open_path,
            commands::reveal_path,
            commands::list_directory,
            commands::list_providers,
            commands::upsert_provider,
            commands::delete_provider,
            commands::activate_provider,
            commands::set_session_provider,
            commands::test_active_provider_connection,
            commands::openrouter_context_length,
            commands::openrouter_credits,
            commands::list_local_models,
            commands::get_local_engine_status,
            commands::get_models_disk_free,
            commands::delete_local_model,
            commands::download_model,
            commands::list_mcp_servers,
            commands::add_mcp_server,
            commands::update_mcp_server,
            commands::delete_mcp_server,
            commands::set_mcp_server_enabled,
            commands::connect_mcp_server,
            commands::get_settings_target,
            commands::list_themes,
            commands::read_user_theme,
            commands::open_themes_folder,
            commands::read_image_data_url,
            commands::validate_setup,
            commands::open_wizard,
            commands::get_wizard_mode,
            commands::complete_setup,
            #[cfg(windows)]
            commands::get_autostart,
            #[cfg(windows)]
            commands::set_autostart,
            commands::get_adaptive_pathway_mcp_status,
            commands::set_adaptive_pathway_enabled,
            commands::get_pathway_beliefs,
            commands::get_pathway_stats,
            commands::delete_pathway_belief,
            commands::set_pathway_session_paused,
            commands::get_kitty_wasm_enabled,
            commands::set_kitty_wasm_enabled,
            commands::get_visualizations_enabled,
            commands::set_visualizations_enabled,
            commands::get_kitty_tools_enabled,
            commands::set_kitty_tools_enabled,
            commands::get_kitty_web_enabled,
            commands::set_kitty_web_enabled,
            commands::get_brave_mcp_search_status,
            commands::set_brave_mcp_search_api_key,
            commands::set_brave_mcp_search_enabled,
        ])
        .setup(move |app| {
            let handle = app.handle();
            // Overlay + tray + global hotkey are the desktop summon path.
            // Android has no always-on-top-over-other-apps window, no tray,
            // and no OS-wide shortcut — it boots straight into the hub
            // (docs/ANDROID.md D9/D23, §8.2).
            #[cfg(desktop)]
            {
                windows::create_overlay(handle)?;
                tray::create(handle)?;
                if let Err(e) = hotkey::register(
                    handle,
                    &hotkeys,
                    clipboard_hotkey.as_deref(),
                    open_window_hotkey.as_deref(),
                ) {
                    tracing::error!("global hotkey registration failed: {e}");
                }
            }
            // First launch: show the setup wizard instead of the (hidden) overlay.
            if !wizard::setup_completed(handle) {
                let _ = windows::open_wizard(handle, "setup");
            }
            // Start Ollama + BigTiny and the health loop in the background.
            // `lifecycle::start_stack` self-heals the bundled plugins'
            // MCP-server registrations once the daemon is up — see
            // `bigtiny::mcp::ensure_builtin_servers`.
            lifecycle::start_stack(handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Kitty application")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // Kill only the children we spawned.
                lifecycle::shutdown(app);
            }
        });
}
