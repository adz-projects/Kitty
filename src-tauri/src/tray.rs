//! System tray icon + menu — the app's persistent visible presence.
//! Menu: Toggle Overlay, Open Chat Window, New Session, Ask about clipboard,
//! Open Settings, Quit.
//! Left-click also toggles the overlay (or focuses the main window if it's open).

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter};

use crate::{hotkey, windows};

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle_overlay", "Toggle Overlay", true, None::<&str>)?;
    // Opens the full chat window directly — no overlay involved at all (owner
    // ask: a way to reach the chat window without summoning the overlay first).
    let open_main = MenuItem::with_id(app, "open_main", "Open Chat Window", true, None::<&str>)?;
    let new_session = MenuItem::with_id(app, "new_session", "New Session", true, None::<&str>)?;
    let ask_clipboard = MenuItem::with_id(
        app,
        "ask_clipboard",
        "Ask about clipboard",
        true,
        None::<&str>,
    )?;
    let scheduled_tasks = MenuItem::with_id(
        app,
        "scheduled_tasks",
        "Scheduled Tasks…",
        true,
        None::<&str>,
    )?;
    let settings = MenuItem::with_id(app, "open_settings", "Open Settings", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &open_main,
            &new_session,
            &ask_clipboard,
            &scheduled_tasks,
            &sep,
            &settings,
            &sep,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("Kitty")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle_overlay" => {
                let _ = windows::toggle_or_focus_main(app);
            }
            "open_main" => {
                let _ = windows::open_main(app);
            }
            "new_session" => {
                let _ = windows::show_overlay(app);
                // Ask the overlay to start a fresh session.
                let _ = app.emit("session://new", ());
            }
            "ask_clipboard" => {
                hotkey::attach_clipboard(app);
            }
            "scheduled_tasks" => {
                let _ = windows::open_settings(app, Some("scheduled_tasks".to_string()), None);
            }
            "open_settings" => {
                let _ = windows::open_settings(app, None, None);
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
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
