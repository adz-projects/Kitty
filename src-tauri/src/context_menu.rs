//! Kitty's own right-click menu, and no browser keys (after Bacon's).
//!
//! WebView2's built-in menu (Back, Reload, Save as, Print…) never shows. Each
//! window's `ContextMenuRequested` is handled here: the native menu is
//! suppressed, the request is held open (a deferral), and the native items
//! worth keeping — spelling suggestions, Add to dictionary, Cut/Copy/Paste,
//! Paste as plain text, Select all — are sent to the window as
//! `context-menu://requested`. The frontend (`lib/contextMenu.ts`) decides
//! whether there is a menu at all and draws it; when the user picks a native
//! item, `context_menu_choose` hands its command id back so WebView2 runs it
//! itself (a real paste event, so the editor's clipboard path still runs; a
//! real spelling replacement). Every close answers, with `None` when nothing
//! was picked.
//!
//! The browser's accelerator keys (Ctrl+F find, Ctrl+P print, Ctrl+R/F5
//! reload, Ctrl+U, DevTools) are switched off too; editing keys stay.
//! Windows only: elsewhere the frontend opens its menu from the DOM event
//! (Android keeps the system's long-press).

use serde::Serialize;

/// A native item the frontend may offer.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NativeItem {
    pub command_id: i32,
    /// WebView2's stable name: `spellCheck`, `addToDictionary`, `cut`,
    /// `copy`, `paste`, `pasteAndMatchStyle`, `selectAll`, `inspectElement`.
    pub name: String,
    pub label: String,
    pub enabled: bool,
}

/// What the window is told about one right-click.
#[derive(Debug, Clone, Serialize)]
pub struct MenuRequest {
    pub editable: bool,
    pub has_selection: bool,
    pub items: Vec<NativeItem>,
}

/// Which native items are passed on. DevTools only in debug builds.
pub fn keep(name: &str, debug: bool) -> bool {
    matches!(
        name,
        "spellCheck"
            | "addToDictionary"
            | "cut"
            | "copy"
            | "paste"
            | "pasteAndMatchStyle"
            | "selectAll"
    ) || (debug && name == "inspectElement")
}

/// A native label without its access-key ampersands (`&Copy` → `Copy`,
/// `&&` → `&`).
pub fn clean_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2ContextMenuItem, ICoreWebView2ContextMenuRequestedEventArgs,
        ICoreWebView2Deferral, ICoreWebView2Settings3, ICoreWebView2_11,
        COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND, COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND,
    };
    use webview2_com::{take_pwstr, ContextMenuRequestedEventHandler};
    use windows_core::{Interface, BOOL, PWSTR};

    use super::{clean_label, keep, MenuRequest, NativeItem};

    /// The requests held open, by window label (UI thread only: COM objects).
    struct Pending {
        args: ICoreWebView2ContextMenuRequestedEventArgs,
        deferral: ICoreWebView2Deferral,
        seq: u64,
    }

    thread_local! {
        static PENDING: RefCell<HashMap<String, Pending>> = RefCell::new(HashMap::new());
        static SEQ: RefCell<u64> = const { RefCell::new(0) };
    }

    /// A held request is answered on its own after this long.
    const TIMEOUT: Duration = Duration::from_secs(30);

    /// Set up one window: browser keys off, the menu bridged.
    pub fn install(window: &WebviewWindow) {
        let app = window.app_handle().clone();
        let label = window.label().to_string();
        let result = window.with_webview(move |webview| unsafe {
            if let Err(e) = setup(&webview.controller(), app, label) {
                tracing::warn!("could not set up the context menu bridge: {e}");
            }
        });
        if let Err(e) = result {
            tracing::warn!("could not reach the webview to set up its menu: {e}");
        }
    }

    unsafe fn setup(
        controller: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
        app: AppHandle,
        label: String,
    ) -> windows_core::Result<()> {
        let core = controller.CoreWebView2()?;
        if let Ok(settings) = core.Settings()?.cast::<ICoreWebView2Settings3>() {
            settings.SetAreBrowserAcceleratorKeysEnabled(false)?;
        }
        let core11 = core.cast::<ICoreWebView2_11>()?;
        let mut token = 0i64;
        core11.add_ContextMenuRequested(
            &ContextMenuRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    on_request(&app, &label, args)?;
                }
                Ok(())
            })),
            &mut token,
        )?;
        Ok(())
    }

    unsafe fn on_request(
        app: &AppHandle,
        label: &str,
        args: ICoreWebView2ContextMenuRequestedEventArgs,
    ) -> windows_core::Result<()> {
        // The native menu never shows; Kitty's is drawn by the page.
        args.SetHandled(true)?;
        let deferral = args.GetDeferral()?;
        let target = args.ContextMenuTarget()?;
        let mut editable = BOOL::default();
        target.IsEditable(&mut editable)?;
        let mut has_selection = BOOL::default();
        target.HasSelection(&mut has_selection)?;
        let request = MenuRequest {
            editable: editable.as_bool(),
            has_selection: has_selection.as_bool(),
            items: native_items(&args).unwrap_or_default(),
        };
        // A request still open (a second right-click) is answered first.
        finish(label, None);
        let seq = SEQ.with(|s| {
            *s.borrow_mut() += 1;
            *s.borrow()
        });
        PENDING.with(|p| {
            p.borrow_mut().insert(
                label.to_string(),
                Pending {
                    args,
                    deferral,
                    seq,
                },
            )
        });
        if app
            .emit_to(label, "context-menu://requested", &request)
            .is_err()
        {
            finish(label, None);
            return Ok(());
        }
        let (app2, label2) = (app.clone(), label.to_string());
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(TIMEOUT).await;
            let label3 = label2.clone();
            let _ = app2.run_on_main_thread(move || finish_if(&label3, seq));
        });
        Ok(())
    }

    unsafe fn native_items(
        args: &ICoreWebView2ContextMenuRequestedEventArgs,
    ) -> windows_core::Result<Vec<NativeItem>> {
        let items = args.MenuItems()?;
        let mut count = 0u32;
        items.Count(&mut count)?;
        let mut out = Vec::new();
        for i in 0..count {
            let item: ICoreWebView2ContextMenuItem = items.GetValueAtIndex(i)?;
            let mut kind = COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND::default();
            item.Kind(&mut kind)?;
            if kind != COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND_COMMAND {
                continue;
            }
            let mut name = PWSTR::null();
            item.Name(&mut name)?;
            let name = take_pwstr(name);
            if !keep(&name, cfg!(debug_assertions)) {
                tracing::trace!("native menu item left out: {name}");
                continue;
            }
            let mut label = PWSTR::null();
            item.Label(&mut label)?;
            let mut command_id = 0i32;
            item.CommandId(&mut command_id)?;
            let mut enabled = BOOL::default();
            item.IsEnabled(&mut enabled)?;
            out.push(NativeItem {
                command_id,
                name,
                label: clean_label(&take_pwstr(label)),
                enabled: enabled.as_bool(),
            });
        }
        Ok(out)
    }

    /// Answer the window's open request: run `command` (a native item), or
    /// nothing.
    pub fn finish(label: &str, command: Option<i32>) {
        let pending = PENDING.with(|p| p.borrow_mut().remove(label));
        if let Some(p) = pending {
            unsafe {
                if let Some(id) = command {
                    let _ = p.args.SetSelectedCommandId(id);
                }
                let _ = p.deferral.Complete();
            }
        }
    }

    fn finish_if(label: &str, seq: u64) {
        let same = PENDING.with(|p| p.borrow().get(label).is_some_and(|x| x.seq == seq));
        if same {
            finish(label, None);
        }
    }
}

#[cfg(windows)]
pub use imp::install;

/// Not Windows: nothing to bridge (the frontend uses the DOM event).
#[cfg(not(windows))]
pub fn install(_window: &tauri::WebviewWindow) {}

/// The frontend's answer to `context-menu://requested`: the native item the
/// user picked, or `None` (Kitty's own item, or the menu was closed).
#[tauri::command]
pub async fn context_menu_choose(
    window: tauri::WebviewWindow,
    command_id: Option<i32>,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        let label = window.label().to_string();
        window
            .run_on_main_thread(move || imp::finish(&label, command_id))
            .map_err(|e| {
                tracing::warn!("could not answer the context menu: {e}");
                "The menu could not be closed".to_string()
            })?;
    }
    #[cfg(not(windows))]
    {
        let _ = (window, command_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_editing_and_spelling_items_are_kept() {
        for name in [
            "spellCheck",
            "addToDictionary",
            "cut",
            "copy",
            "paste",
            "pasteAndMatchStyle",
            "selectAll",
        ] {
            assert!(keep(name, false), "{name}");
        }
        for name in [
            "back", "forward", "reload", "saveAs", "print", "emoji", "share", "other",
        ] {
            assert!(!keep(name, true), "{name}");
        }
        assert!(!keep("inspectElement", false));
        assert!(keep("inspectElement", true));
    }

    #[test]
    fn access_key_ampersands_are_dropped() {
        assert_eq!(clean_label("&Copy"), "Copy");
        assert_eq!(clean_label("Paste &as plain text"), "Paste as plain text");
        assert_eq!(clean_label("Salt && pepper"), "Salt & pepper");
        assert_eq!(clean_label("receive"), "receive");
    }
}
