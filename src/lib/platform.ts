// Which platform this webview is running on (docs/ANDROID.md §8.2/§8.4).
//
// Read from the user-agent rather than through a Tauri command or the `os`
// plugin, for one reason: the answer is needed *synchronously, before first
// paint*, to stamp `data-platform` on the document root. Everything gated on
// that attribute — safe-area insets, the mobile shell, touch sizing — would
// otherwise flash the desktop layout for however long an IPC round-trip takes.
//
// The narrowness is deliberate. This is not a general capability check: use it
// only for layout decisions that must be made before the first frame. Anything
// that can wait, and anything about what the *backend* supports, belongs in a
// command where Rust's own `cfg!` is the authority.

/** True on Android's WebView. Every Android WebView UA contains "Android";
    no desktop WebView2/WebKit UA does. */
export function isAndroid(): boolean {
  return typeof navigator !== 'undefined' && /android/i.test(navigator.userAgent);
}

export type Platform = 'android' | 'desktop';

export function currentPlatform(): Platform {
  return isAndroid() ? 'android' : 'desktop';
}

/** Stamp the platform on `<html>` so CSS can branch on it. Call before first
    paint. Returns the platform for callers that also want it in JS. */
export function applyPlatformAttribute(): Platform {
  const p = currentPlatform();
  document.documentElement.setAttribute('data-platform', p);
  return p;
}
