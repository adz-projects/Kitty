//! System tray icon + menu — the app's persistent visible presence.
//! Menu: Toggle Overlay, New Window, Settings, active background overlay runs, Quit.
//! Left-click also toggles the overlay (or focuses the main window if it's open).

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::AppHandle;

use crate::windows;

fn build_menu(app: &AppHandle, bg_runs: &[(String, String)]) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    let toggle = MenuItem::with_id(app, "toggle_overlay", "Toggle Overlay", true, None::<&str>)?;
    // A new chat window with its own fresh chat.
    let new_window = MenuItem::with_id(app, "new_window", "New Window", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "open_settings", "Settings", true, None::<&str>)?;
    menu.append_items(&[&toggle, &new_window, &settings])?;

    if !bg_runs.is_empty() {
        let bg_sep = PredefinedMenuItem::separator(app)?;
        menu.append(&bg_sep)?;
        for (session_id, preview) in bg_runs {
            let label = format!("Thinking: {preview}");
            let open_item = MenuItem::with_id(
                app,
                format!("bg_open:{session_id}"),
                "Open Window Now",
                true,
                None::<&str>,
            )?;
            let cancel_item = MenuItem::with_id(
                app,
                format!("bg_cancel:{session_id}"),
                "Cancel Run",
                true,
                None::<&str>,
            )?;
            let sub = SubmenuBuilder::with_id(app, format!("bg_run:{session_id}"), label)
                .item(&open_item)
                .item(&cancel_item)
                .build()?;
            menu.append(&sub)?;
        }
    }

    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    menu.append_items(&[&sep, &quit])?;
    Ok(menu)
}

pub fn update_menu(app: &AppHandle, bg_runs: &[(String, String)]) {
    let app_main = app.clone();
    let runs_owned = bg_runs.to_vec();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app_main.tray_by_id("main-tray") {
            match build_menu(&app_main, &runs_owned) {
                Ok(menu) => {
                    let _ = tray.set_menu(Some(menu));
                }
                Err(e) => {
                    tracing::warn!("failed to rebuild tray menu: {e}");
                }
            }
        }
    });
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let bg_runs = crate::overlay_runs::unrevealed_tray_runs(app);
    let menu = build_menu(app, &bg_runs)?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("Kitty")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if let Some(session_id) = id.strip_prefix("bg_open:") {
                crate::overlay_runs::reveal_now(app, session_id);
                return;
            }
            if let Some(session_id) = id.strip_prefix("bg_cancel:") {
                crate::overlay_runs::cancel_background_run(app, session_id);
                return;
            }
            match id {
                "toggle_overlay" => {
                    let _ = windows::toggle_or_focus_main(app);
                }
                "new_window" => {
                    let _ = windows::open_new_chat_window(app, None);
                }
                "open_settings" => {
                    let _ = windows::open_settings(app, None, None);
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = windows::toggle_or_focus_main(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}
