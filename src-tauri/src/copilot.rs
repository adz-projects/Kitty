//! Copilot-key capture (Phase 6). A low-level keyboard hook (`WH_KEYBOARD_LL`)
//! on a dedicated thread detects the Copilot chord (`Win+Shift+F23`), swallows it
//! so Windows Copilot does not launch, and toggles the overlay. Feature-flagged
//! behind `use_copilot_key`; the flag defaults ON the first time the chord is
//! observed. Documented fallback: remap the key with PowerToys to the configured
//! accelerator.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_F23, VK_LWIN, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HHOOK,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::config;
use crate::state::AppState;

static APP: OnceLock<AppHandle> = OnceLock::new();
/// Whether the Copilot chord has ever been seen (drives "default ON once observed").
static OBSERVED: AtomicBool = AtomicBool::new(false);

fn key_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState is a simple read of key state.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

/// The hook callback. Runs on the dedicated hook thread for every key event.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if kb.vkCode == VK_F23.0 as u32
                && (key_down(VK_LWIN.0) || key_down(VK_RWIN.0))
                && key_down(VK_SHIFT.0)
                && on_chord()
            {
                // Swallow the chord so Windows Copilot doesn't launch.
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Handle a detected Copilot chord. Returns true if we handled (and should
/// swallow) it. On first observation the setting is enabled and persisted.
fn on_chord() -> bool {
    let app = match APP.get() {
        Some(a) => a.clone(),
        None => return false,
    };

    let first = !OBSERVED.swap(true, Ordering::SeqCst);
    let enabled = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        if first && !cfg.use_copilot_key {
            cfg.use_copilot_key = true;
            let _ = config::save(&cfg);
        }
        cfg.use_copilot_key
    };

    if !enabled {
        return false;
    }
    // Toggle on the main thread (window ops must not run on the hook thread).
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = crate::windows::toggle_overlay(&app2) {
            tracing::warn!("copilot toggle failed: {e}");
        }
    });
    true
}

/// Install the low-level keyboard hook on a dedicated thread with a message loop
/// (required for `WH_KEYBOARD_LL` to deliver events). Safe to call once.
pub fn install(app: &AppHandle) {
    if APP.set(app.clone()).is_err() {
        return; // already installed
    }
    std::thread::spawn(|| unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0);
        if hook.is_err() {
            tracing::warn!("failed to install Copilot keyboard hook");
            return;
        }
        tracing::info!("Copilot keyboard hook installed");
        // Pump messages so the hook stays alive and fires.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}
