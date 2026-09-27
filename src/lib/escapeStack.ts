// Escape for in-page layers (dialogs, popovers): the topmost open layer
// closes first, and only when none is open does Escape reach the window's own
// handler (the overlay hides itself on Escape). Same stacking idea as
// `backDismiss.ts`, for the keyboard instead of Android's Back button.
//
// One capture-phase listener on `window`, so it runs before any bubble-phase
// handler, and stopping propagation there keeps the overlay from hiding while
// a popover is still open.

const layers: Array<() => void> = [];
let listening = false;

function onKeyDown(e: KeyboardEvent) {
  if (e.key !== 'Escape' || layers.length === 0) return;
  e.stopPropagation();
  e.preventDefault();
  layers[layers.length - 1]();
}

/** Register an open layer; returns its unregister. The last registered
    closes first. */
export function registerEscapeLayer(close: () => void): () => void {
  if (!listening) {
    window.addEventListener('keydown', onKeyDown, true);
    listening = true;
  }
  layers.push(close);
  return () => {
    const i = layers.lastIndexOf(close);
    if (i !== -1) layers.splice(i, 1);
  };
}

/** Whether any layer is open (for tests and for handlers that must defer). */
export function hasEscapeLayer(): boolean {
  return layers.length > 0;
}
