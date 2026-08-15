// Keep the app box the size of the *visible* area when the keyboard is open.
//
// `android:windowSoftInputMode="adjustResize"` is set in the manifest and is
// still correct, but on Android 15+ it is not sufficient on its own: an app
// targeting SDK 35+ is edge-to-edge, and the IME arrives as an inset rather
// than as a window resize. Measured on a Pixel 10 Pro (API 36) with the
// keyboard open:
//
//   document.documentElement.clientHeight  1083   <- layout viewport, unchanged
//   visualViewport.height                   661   <- what you can actually see
//   visualViewport.offsetTop                422   <- how far it scrolled away
//
// So the layout kept its full height and the browser simply scrolled the
// visible window down to follow the caret, carrying the header 332px off the
// top of the screen. That is the "everything shoves up" symptom: nothing
// compressed, it just moved.
//
// Setting a CSS height on `html` is not enough: that does not shrink the
// initial containing block, so `documentElement.clientHeight` stayed 1083 and
// the browser kept its 422px visual scroll. What works is pinning the app box
// to the visible rectangle outright — `position: fixed` at `--app-top`, with
// height `--app-h` — so the chrome stays put and only the conversation gets
// shorter, which is what a keyboard should do.

/** Track the visual viewport and publish it as `--app-h` / `--app-top`.
 *
 * Returns a cleanup function. No-ops where `visualViewport` is unavailable,
 * leaving the CSS fallback (`100%`) in charge. */
export function trackViewportHeight(): () => void {
  const vv = typeof window !== 'undefined' ? window.visualViewport : undefined;
  if (!vv) return () => {};

  const apply = () => {
    // Round down: a fractional height can leave a sub-pixel gap that shows as
    // a 1px sliver of the page behind the keyboard.
    const root = document.documentElement.style;
    root.setProperty('--app-h', `${Math.floor(vv.height)}px`);
    root.setProperty('--app-top', `${Math.round(vv.offsetTop)}px`);
  };

  apply();
  vv.addEventListener('resize', apply);
  // `scroll` matters as much as `resize`: the visual viewport can slide
  // without changing size (the caret moving between fields), and the app box
  // needs to stay pinned to what is on screen.
  vv.addEventListener('scroll', apply);
  return () => {
    vv.removeEventListener('resize', apply);
    vv.removeEventListener('scroll', apply);
    document.documentElement.style.removeProperty('--app-h');
    document.documentElement.style.removeProperty('--app-top');
  };
}
