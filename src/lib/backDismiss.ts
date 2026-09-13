// Android's Back button for in-page layers (the menu drawer, the artifacts
// sheet, the Settings route).
//
// `WryActivity` maps Back to `webView.goBack()` whenever the WebView has
// history, and falls through to the system (leaving the app) when it doesn't.
// So a layer that should close on Back needs a history entry to consume. This
// keeps exactly one "guard" entry on the stack while any layer is open, and
// none when nothing is:
//
// - Back pops the guard → the topmost layer closes → if more layers remain, a
//   fresh guard is pushed for them.
// - A layer closed some other way (tapping the scrim, a swipe) while it was the
//   last one leaves a stale guard behind, which would make the *next* Back do
//   nothing visible. So we `history.back()` it away ourselves, and swallow the
//   `popstate` that produces.
//
// `history.back()` is asynchronous, which is why reconciliation is skipped
// while one is in flight and redone from the `popstate` handler: closing the
// drawer and opening Settings in the same tick must end with one guard, not
// zero (the queued back popping Settings' fresh guard) or two.

interface Layer {
  close: () => void;
}

const GUARD_KEY = 'kittyBackGuard';
const layers: Layer[] = [];
let pendingBacks = 0;
let listening = false;

function atGuard(): boolean {
  const state: unknown = history.state;
  return typeof state === 'object' && state !== null && GUARD_KEY in state;
}

function reconcile() {
  if (pendingBacks > 0) return;
  const guard = atGuard();
  if (layers.length > 0 && !guard) {
    history.pushState({ [GUARD_KEY]: true }, '');
  } else if (layers.length === 0 && guard) {
    pendingBacks++;
    history.back();
  }
}

function onPopState() {
  if (pendingBacks > 0) {
    pendingBacks--;
  } else {
    // A real Back press: it consumed the guard; close whatever is on top.
    // Removed from the stack *before* closing — the layer's own unregister
    // only runs later, from a React effect cleanup, and until then the
    // reconcile below would count it as still open and push a guard for it.
    // The late unregister then finds nothing and is a no-op.
    layers.pop()?.close();
  }
  reconcile();
}

/** Register an open layer. Returns the unregister function to call when the
    layer closes by any means. Layers stack: the last registered closes first. */
export function registerBackLayer(close: () => void): () => void {
  if (!listening) {
    window.addEventListener('popstate', onPopState);
    listening = true;
  }
  const layer: Layer = { close };
  layers.push(layer);
  reconcile();
  return () => {
    const i = layers.indexOf(layer);
    if (i === -1) return;
    layers.splice(i, 1);
    reconcile();
  };
}
