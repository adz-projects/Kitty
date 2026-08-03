//! Kitty — Tauri v2 application entry point.
//!
//! One Rust process owns four labelled windows, the BigTiny + Ollama process
//! lifecycle, config, tray, and the global hotkey. All I/O lives here; the
//! webview only talks to us through the commands registered below.

mod adaptive_pathway;
mod bigtiny;
mod commands;
mod config;
mod hotkey;
mod lifecycle;
mod log_capture;
mod notifications;
mod ollama;
mod openrouter;
mod screenshot;
mod state;
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

    let cfg = config::load().unwrap_or_else(|e| {
        tracing::warn!("config load failed ({e}); using defaults");
        config::Config::default()
    });
    // One-time migration from the pre-rename `goose-overlay` keyring service
    // (config.json itself already migrated by `config::load` -> `config_dir`).
    let provider_ids: Vec<String> = cfg.providers.iter().map(|p| p.id.clone()).collect();
    config::providers::migrate_secrets(&provider_ids);
    let hotkeys = cfg.hotkeys.clone();
    let clipboard_hotkey = cfg.clipboard_hotkey.clone();
    let open_window_hotkey = cfg.open_window_hotkey.clone();

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
            commands::open_new_chat_window,
            commands::capture_screenshot_region,
            commands::get_screenshot_preview,
            commands::report_screenshot_selection,
            commands::cancel_screenshot_selection,
            commands::get_pending_handoff,
            commands::get_stack_status,
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
            commands::test_active_provider_connection,
            commands::openrouter_context_length,
            commands::openrouter_credits,
            commands::ollama_list_models,
            commands::ollama_delete_model,
            commands::ollama_show_context_length,
            commands::ollama_pull_model,
            commands::read_ollama_env,
            commands::set_ollama_env,
            commands::restart_ollama,
            commands::ensure_ollama_running,
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
            commands::detect_dependencies,
            commands::install_dependency,
            commands::validate_setup,
            commands::open_wizard,
            commands::get_wizard_mode,
            commands::complete_setup,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_adaptive_pathway_status,
            commands::get_adaptive_pathway_embedding_status,
            commands::get_adaptive_pathway_mcp_status,
            commands::restart_adaptive_pathway,
            commands::set_adaptive_pathway_enabled,
            commands::adaptive_pathway_get_edge,
            commands::adaptive_pathway_get_state,
            commands::adaptive_pathway_get_metrics,
            commands::adaptive_pathway_record_annotation,
            commands::adaptive_pathway_toggle_suggestions,
            commands::adaptive_pathway_get_schism,
            commands::adaptive_pathway_resolve_schism,
            commands::adaptive_pathway_update_ensemble_weights,
            commands::adaptive_pathway_health,
            commands::adaptive_pathway_graph_health,
            commands::adaptive_pathway_list_domains,
            commands::adaptive_pathway_update_domain,
            commands::adaptive_pathway_accept_nudge,
            commands::adaptive_pathway_dismiss_nudge,
            commands::adaptive_pathway_get_session_reflection,
            commands::get_wasm_math_mcp_enabled,
            commands::set_wasm_math_mcp_enabled,
            commands::get_visualizations_enabled,
            commands::set_visualizations_enabled,
            commands::get_kitty_tools_enabled,
            commands::set_kitty_tools_enabled,
            commands::get_kitty_docs_web_enabled,
            commands::set_kitty_docs_web_enabled,
            commands::get_brave_mcp_search_status,
            commands::set_brave_mcp_search_api_key,
            commands::set_brave_mcp_search_enabled,
        ])
        .setup(move |app| {
            let handle = app.handle();
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
