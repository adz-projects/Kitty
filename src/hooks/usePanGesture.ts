import { useEffect, useRef, type RefObject } from 'react';

/** Movement (px) before a touch commits to an axis. Below this a touch is
    still a tap, and deciding early would steal vertical scrolls whose first
    few pixels happened to wobble sideways. */
const LOCK_SLOP = 8;
/** Window (ms) of recent samples the release velocity is measured over. */
const VELOCITY_WINDOW_MS = 100;

export interface PanStart {
  target: Element;
  /** Displacement at the moment the axis locked, so a caller can accept only
      one direction (e.g. "rightward swipes open the drawer"). */
  dx: number;
  dy: number;
}

export interface PanGestureOptions {
  axis: 'x' | 'y';
  enabled?: boolean;
  /** Called once, when the touch locks onto `axis`. Return false to let the
      touch behave normally (scroll, tap) for the rest of its life. */
  shouldStart: (start: PanStart) => boolean;
  /** Signed displacement along `axis` since touch-down. */
  onMove: (delta: number) => void;
  /** Final displacement and velocity (px/ms, same sign convention). */
  onEnd: (delta: number, velocity: number) => void;
}

/** One-axis touch drag, for panels that follow the finger (Android shell).
 *
 * Native listeners rather than React's synthetic ones: `touchmove` has to be
 * non-passive to `preventDefault()` a claimed gesture (otherwise the page
 * scrolls underneath the panel being dragged), and React registers touch
 * listeners as passive. `preventDefault` is only ever called *after* the
 * touch has locked onto this gesture's axis and `shouldStart` accepted it, so
 * everything else — scrolling, taps, text selection — is untouched.
 *
 * `target` is a ref to the element to listen on, or `window` for a gesture
 * that may start anywhere. Callbacks are read through a ref, so passing
 * inline functions doesn't re-attach the listeners every render. */
export function usePanGesture(
  target: RefObject<HTMLElement | null> | Window,
  options: PanGestureOptions
) {
  const optsRef = useRef(options);
  optsRef.current = options;
  const enabled = options.enabled ?? true;

  useEffect(() => {
    if (!enabled) return;
    const el = target instanceof Window ? target : target.current;
    if (!el) return;

    let state: 'idle' | 'pending' | 'active' | 'ignored' = 'idle';
    let x0 = 0;
    let y0 = 0;
    let delta = 0;
    let samples: { t: number; d: number }[] = [];

    const onStart = (e: Event) => {
      const te = e as TouchEvent;
      if (te.touches.length !== 1) {
        state = 'ignored';
        return;
      }
      x0 = te.touches[0].clientX;
      y0 = te.touches[0].clientY;
      delta = 0;
      samples = [];
      state = 'pending';
    };

    const onMove = (e: Event) => {
      const te = e as TouchEvent;
      if (state === 'idle' || state === 'ignored') return;
      const t = te.touches[0];
      if (!t) return;
      const dx = t.clientX - x0;
      const dy = t.clientY - y0;
      if (state === 'pending') {
        if (Math.max(Math.abs(dx), Math.abs(dy)) < LOCK_SLOP) return;
        const locked = Math.abs(dx) > Math.abs(dy) ? 'x' : 'y';
        const opts = optsRef.current;
        if (
          locked !== opts.axis ||
          !(te.target instanceof Element) ||
          !opts.shouldStart({ target: te.target, dx, dy })
        ) {
          state = 'ignored';
          return;
        }
        state = 'active';
      }
      if (te.cancelable) te.preventDefault();
      delta = optsRef.current.axis === 'x' ? dx : dy;
      const now = performance.now();
      samples.push({ t: now, d: delta });
      while (samples.length > 2 && now - samples[0].t > VELOCITY_WINDOW_MS) samples.shift();
      optsRef.current.onMove(delta);
    };

    const onEnd = () => {
      if (state === 'active') {
        let velocity = 0;
        if (samples.length >= 2) {
          const first = samples[0];
          const last = samples[samples.length - 1];
          const dt = last.t - first.t;
          if (dt > 0) velocity = (last.d - first.d) / dt;
        }
        optsRef.current.onEnd(delta, velocity);
      }
      state = 'idle';
    };

    el.addEventListener('touchstart', onStart, { passive: true });
    el.addEventListener('touchmove', onMove, { passive: false });
    el.addEventListener('touchend', onEnd);
    el.addEventListener('touchcancel', onEnd);
    return () => {
      el.removeEventListener('touchstart', onStart);
      el.removeEventListener('touchmove', onMove);
      el.removeEventListener('touchend', onEnd);
      el.removeEventListener('touchcancel', onEnd);
    };
  }, [target, enabled]);
}
