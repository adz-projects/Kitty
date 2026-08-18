//! Kitty — Tauri v2 application entry point.
//!
//! One Rust process owns the overlay, the hub window(s), the BigTiny
//! lifecycle, config, tray, and the global hotkey. All I/O lives here; the
//! webview only talks to us through the commands registered below.

#[cfg(target_os = "android")]
mod android;
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

/// On Android this is the JNI entry point, not just a function `main` calls.
///
/// `mobile_entry_point` generates `Java_<package>_Rust_create`, which
/// `WryActivity.onCreate` invokes through `System.loadLibrary`. Without it the
/// `.so` builds and loads fine and then the app dies instantly with
/// `UnsatisfiedLinkError: No implementation found for void
/// com.kitty.app.Rust.create()` — a failure that reads like a packaging
/// problem rather than a missing attribute. Desktop is unaffected: the `cfg`
/// is false there and `main` calls this directly.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Structured logs to stderr (unchanged); RUST_LOG overrides the default
    // filter. Also captures WARN/ERROR events into an in-memory ring buffer
    // (`log_capture`) that Settings → Advanced's error log reads — same
    // filter applies to both layers via `.with(env_filter)` on the registry.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "kitty_lib=info,warn".into());
    // Android discards a process's stdout, so the `fmt` layer's default writer
    // is a black hole there — including for the messages explaining why
    // startup failed. Route it to logcat instead (`adb logcat -s Kitty:V`).
    // See `android::logcat`.
    #[cfg(target_os = "android")]
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(android::logcat::MakeLogcatWriter);
    #[cfg(not(target_os = "android"))]
    let fmt_layer = tracing_subscriber::fmt::layer();
    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
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

    // `tauri-plugin-notification` is desktop-only, and not by preference —
    // its Android side crashes the app. `NotificationPlugin.onIntent` reads a
    // `lateinit manager` that only `load()` initialises, and `load()` only
    // runs once something invokes the plugin from JS. Nothing here does
    // (notifications are posted from Rust, and Android's own Settings owns
    // the toggles), so `manager` stays uninitialised and **every**
    // `onNewIntent` throws `UninitializedPropertyAccessException` and kills
    // the process. With `launchMode="singleTask"` that means tapping the
    // launcher icon while Kitty is running force-closes it. Reproduced on
    // 2.3.3, which is the latest published version — there is no upgrade to
    // take. Revisit when Phase 7 adds the foreground service, which posts its
    // own notification from Kotlin and doesn't need this plugin.
    #[cfg(not(target_os = "android"))]
    {
        builder = builder.plugin(tauri_plugin_notification::init());
    }

    // Android only, and used only from Rust (`commands::download_file`): the
    // save dialog returns a `content://` URI, and this plugin is what resolves
    // one to a writable file descriptor via the ContentResolver. Its JS
    // commands are registered but unreachable — `capabilities/default.json`
    // grants none of them, so the webview gains no filesystem access from
    // this. Desktop writes the chosen path directly and needs nothing.
    #[cfg(target_os = "android")]
    {
        builder = builder.plugin(tauri_plugin_fs::init());
        // Must come before anything reads a secret — provider registration in
        // `start_stack` does, and this is what backs `keyring::get_secret` on
        // Android. Registered here rather than in `setup` so the plugin's own
        // `setup` runs in the builder's ordering guarantee.
        builder = builder.plugin(android::init());
    }

    builder
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
            commands::get_thinking_effort,
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
            commands::set_session_context_dir,
            commands::reset_session_context_dir,
            commands::set_session_persona_override,
            commands::inspect_paths,
            commands::open_path,
            commands::reveal_path,
            commands::download_file,
            commands::list_directory,
            commands::list_providers,
            commands::upsert_provider,
            commands::delete_provider,
            commands::activate_provider,
            commands::set_session_provider,
            commands::test_active_provider_connection,
            commands::openrouter_context_length,
            commands::ollama_context_length,
            commands::custom_openai_context_length,
            commands::openrouter_credits,
            commands::discover_provider_models,
            commands::discover_provider_models_for_saved,
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
            commands::get_route_target,
            commands::list_themes,
            commands::read_user_theme,
            commands::open_themes_folder,
            commands::validate_setup,
            commands::open_wizard,
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
            // Android's app directories can only come from the Android
            // `Context`, which means they are not knowable until here —
            // `run()` above already tried to load config and failed with
            // "could not resolve the app config directory". Install the real
            // base and re-load, so the rest of startup sees the user's
            // settings rather than defaults.
            //
            // Desktop resolves its paths with `dirs` before the app is even
            // built, so none of this runs there.
            #[cfg(target_os = "android")]
            {
                use tauri::Manager as _;
                match handle.path().app_data_dir() {
                    Ok(dir) => {
                        config::init_app_dir(dir);
                        let (reloaded, _) = config::load_with_recovery();
                        *handle.state::<AppState>().config.lock().unwrap() = reloaded;
                    }
                    Err(e) => {
                        // Nothing below can write: config, the daemon's DB,
                        // and downloaded models all hang off this.
                        tracing::error!("could not resolve the Android app data dir: {e}");
                    }
                }
            }
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
            } else if cfg!(target_os = "android") {
                // ...and every launch after that, on Android, open the hub.
                //
                // Nothing else would. `create_overlay` is desktop-only, and
                // the hub is otherwise only created by the wizard above or by
                // the tray/hotkey/single-instance paths — all desktop. So a
                // completed first run left Android with a live process, a
                // healthy daemon, and **no window at all**: a black screen,
                // with logs that look entirely fine because every subsystem
                // really did start. §8.2's "Android boots straight into the
                // hub" was a statement of intent that nothing implemented.
                if let Err(e) = windows::open_main(handle) {
                    tracing::error!("could not open the hub window: {e}");
                }
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
