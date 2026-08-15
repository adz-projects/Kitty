//! Screenshot region capture commands (Feature 3) — orchestrates the
//! capture-preview -> selection-window -> targeted-final-crop flow described
//! in `screenshot.rs`'s module doc comment. The selection window is plain
//! HTML/canvas UI; only the actual pixel capture touches Win32 APIs.

use std::time::Duration;

use tauri::{AppHandle, Manager, State};
use tokio::sync::oneshot;

use crate::screenshot;
use crate::state::{AppState, ScreenshotRegion};
use crate::windows;

use super::ImageAttachment;

/// Downsampled preview capped at this many pixels on its longer side —
/// plenty for visually picking a region, far smaller than a multi-MB
/// full-resolution capture would be to ship over IPC.
const PREVIEW_MAX_DIMENSION: u32 = 1600;

/// How long `capture_screenshot_region` waits for the user's selection before
/// giving up. The selection sender lives in `AppState.screenshot_selection`;
/// closing the selection window without calling the cancel command never drops
/// it, so without a bound a bare `rx.await` would hang the command forever.
const SELECTION_WAIT: Duration = Duration::from_secs(60);

/// Kick off a screenshot capture: captures a lightweight preview, opens the
/// region-selection window over it, and awaits the user's choice (or
/// cancellation) before doing a fresh, full-resolution, targeted capture of
/// exactly the selected rectangle. Returns the final cropped image, ready to
/// hand to `addPendingImage` exactly like a clipboard-pasted image.
#[tauri::command]
pub async fn capture_screenshot_region(app: AppHandle) -> Result<ImageAttachment, String> {
    // The GDI capture is blocking — run it on a blocking thread, not a tokio
    // worker.
    let (preview, (x, y, w, h)) = tokio::task::spawn_blocking(move || {
        screenshot::capture_full_desktop_preview(PREVIEW_MAX_DIMENSION)
    })
    .await
    .map_err(|e| format!("screenshot capture task panicked: {e}"))??;

    let (tx, rx) = oneshot::channel::<Option<ScreenshotRegion>>();
    {
        let state = app.state::<AppState>();
        *state.screenshot_preview.lock().unwrap() = Some((preview, (x, y, w, h)));
        *state.screenshot_selection.lock().unwrap() = Some(tx);
    }

    if let Err(e) = windows::create_screenshot_select_window(&app, x, y, w, h).await {
        // A failed window build must not leak the MB-scale base64 preview or
        // the orphaned selection sender in AppState — the next capture's
        // state would be polluted by both.
        let state = app.state::<AppState>();
        *state.screenshot_preview.lock().unwrap() = None;
        *state.screenshot_selection.lock().unwrap() = None;
        return Err(e.to_string());
    }
    if let Some(win) = app.get_webview_window(windows::SCREENSHOT_SELECT) {
        let _ = win.show();
        let _ = win.set_focus();
    }

    // Cancellation (the sender dropped without ever sending, e.g. the user
    // closed the window some other way) resolves to `None` here too, same
    // as an explicit Escape. A time-out (the selection window wasn't
    // cancelled but also never reported) is treated the same way so the
    // command can't hang the calling window forever.
    let selection = match tokio::time::timeout(SELECTION_WAIT, rx).await {
        Ok(Ok(sel)) => sel,
        Ok(Err(_)) | Err(_) => None,
    };

    // Drop any live selection sender we did not consume — a stale sender must
    // not hang a later capture's wait.
    app.state::<AppState>()
        .screenshot_selection
        .lock()
        .unwrap()
        .take();

    if let Some(win) = app.get_webview_window(windows::SCREENSHOT_SELECT) {
        let _ = win.close();
    }
    {
        let state = app.state::<AppState>();
        *state.screenshot_preview.lock().unwrap() = None;
    }

    let (sx, sy, sw, sh) = selection.ok_or_else(|| "Screenshot capture cancelled".to_string())?;
    // Full-resolution BitBlt + PNG encode — the same blocking-GDI class as
    // the preview capture at the top of this function, so it gets the same
    // `spawn_blocking` treatment rather than parking an async runtime worker.
    let data_url = tokio::task::spawn_blocking(move || screenshot::capture_region(sx, sy, sw, sh))
        .await
        .map_err(|e| format!("screenshot capture task panicked: {e}"))??;
    Ok(ImageAttachment {
        mime: "image/png".to_string(),
        data_url,
    })
}

/// One-time read of the preview + virtual-screen rect for the
/// region-selection window's own mount effect — `(preview_data_url, x, y,
/// width, height)`, all in physical pixels for `x`/`y`/`width`/`height`.
#[tauri::command]
pub fn get_screenshot_preview(
    state: State<'_, AppState>,
) -> Result<Option<(String, ScreenshotRegion)>, String> {
    Ok(state.screenshot_preview.lock().unwrap().clone())
}

/// The selection window reports the user's chosen rectangle (physical
/// pixels, already translated from its own fractional click coordinates —
/// see the frontend's own translation comment) and wakes the awaiting
/// `capture_screenshot_region` call.
#[tauri::command]
pub fn report_screenshot_selection(
    state: State<'_, AppState>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), String> {
    if let Some(tx) = state.screenshot_selection.lock().unwrap().take() {
        let _ = tx.send(Some((x, y, width, height)));
    }
    Ok(())
}

/// Escape (or any other cancel path) in the selection window.
#[tauri::command]
pub fn cancel_screenshot_selection(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(tx) = state.screenshot_selection.lock().unwrap().take() {
        let _ = tx.send(None);
    }
    Ok(())
}
