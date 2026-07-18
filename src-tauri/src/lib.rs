//! Goose Overlay — Tauri v2 application entry point.
//!
//! One Rust process owns four labelled windows, the goosed + Ollama process
//! lifecycle, config, tray, and the global hotkey. All I/O lives here; the
//! webview only talks to us through the commands registered below.

mod adaptive_pathway;
mod commands;
mod config;
mod goose_config;
mod goosed;
mod hotkey;
mod lifecycle;
mod log_capture;
mod notifications;
mod ollama;
mod openrouter;
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
        .unwrap_or_else(|_| "goose_overlay_lib=info,warn".into());
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(log_capture::CaptureLayer)
        .with(env_filter)
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
            commands::is_session_busy,
            commands::set_active_session,
            commands::get_active_session,
            commands::respond_permission,
            commands::set_mode,
            commands::list_sessions,
            commands::load_session,
            commands::delete_session,
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
            commands::inspect_paths,
            commands::open_path,
            commands::reveal_path,
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
            commands::list_default_extensions,
            commands::set_default_extension_enabled,
            commands::add_extension,
            commands::set_extension_env,
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
            commands::adaptive_pathway_list_domains,
            commands::adaptive_pathway_update_domain,
            commands::adaptive_pathway_accept_nudge,
            commands::adaptive_pathway_dismiss_nudge,
            commands::adaptive_pathway_get_session_reflection,
            commands::get_replacement_mcp_enabled,
            commands::set_replacement_mcp_enabled,
            commands::disable_builtin_dev_extensions,
        ])
        .setup(move |app| {
            let handle = app.handle();
            windows::create_overlay(handle)?;
            tray::create(handle)?;
            if let Err(e) = hotkey::register(handle, &hotkeys, clipboard_hotkey.as_deref()) {
                tracing::error!("global hotkey registration failed: {e}");
            }
            // First launch: show the setup wizard instead of the (hidden) overlay.
            if !wizard::setup_completed(handle) {
                let _ = windows::open_wizard(handle, "setup");
            }
            // Keep the replacement-mcp extension's config.yaml entry pointed at
            // this install's bundled exe (self-heals across an update/reinstall,
            // same rationale as Adaptive Pathway's env-var migration below).
            // Best-effort: a fresh install's config.yaml may not have an
            // `extensions` map yet if goose has literally never run once.
            if let Err(e) = commands::replacement_mcp::ensure_registered() {
                tracing::warn!("replacement-mcp registration self-heal failed: {e}");
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
