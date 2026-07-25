import { useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';

interface VirtualRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface DragState {
  startX: number;
  startY: number;
  curX: number;
  curY: number;
}

/** Full-desktop region-selection overlay (Feature 3 — screenshot capture).
    Shows a downsampled preview of the desktop (fetched once on mount) as a
    dimmed background, stretched to exactly fill this window — the window
    itself is sized/positioned in *physical* pixels to the full
    virtual-screen rect (`windows.rs::create_screenshot_select_window`), so a
    selection rectangle's *fraction* of this window's own CSS width/height
    maps directly back to the same fraction of the real screen, with no
    `devicePixelRatio` arithmetic needed — this sidesteps per-monitor DPI
    differences within one virtual desktop entirely, rather than trying to
    account for each monitor's own scale factor individually. */
export function App() {
  const [preview, setPreview] = useState<string | null>(null);
  const [rect, setRect] = useState<VirtualRect | null>(null);
  const [drag, setDrag] = useState<DragState | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void ipc.getScreenshotPreview().then((data) => {
      if (!data) return;
      const [url, x, y, w, h] = data;
      setPreview(url);
      setRect({ x, y, w, h });
    });
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') void ipc.cancelScreenshotSelection();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);

  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    setDrag({ startX: e.clientX, startY: e.clientY, curX: e.clientX, curY: e.clientY });
  };

  const onMouseMove = (e: React.MouseEvent) => {
    if (!drag) return;
    setDrag({ ...drag, curX: e.clientX, curY: e.clientY });
  };

  const onMouseUp = () => {
    if (!drag) return;
    const container = containerRef.current;
    const left = Math.min(drag.startX, drag.curX);
    const top = Math.min(drag.startY, drag.curY);
    const width = Math.abs(drag.curX - drag.startX);
    const height = Math.abs(drag.curY - drag.startY);
    setDrag(null);
    // A tiny/accidental click isn't a real selection — keep the overlay open
    // rather than reporting a near-zero-size region.
    if (!rect || !container || width < 4 || height < 4) return;

    const fracX = left / container.clientWidth;
    const fracY = top / container.clientHeight;
    const fracW = width / container.clientWidth;
    const fracH = height / container.clientHeight;
    const px = rect.x + Math.round(fracX * rect.w);
    const py = rect.y + Math.round(fracY * rect.h);
    const pw = Math.max(1, Math.round(fracW * rect.w));
    const ph = Math.max(1, Math.round(fracH * rect.h));
    void ipc.reportScreenshotSelection(px, py, pw, ph);
  };

  const selection = drag
    ? {
        left: Math.min(drag.startX, drag.curX),
        top: Math.min(drag.startY, drag.curY),
        width: Math.abs(drag.curX - drag.startX),
        height: Math.abs(drag.curY - drag.startY),
      }
    : null;

  return (
    <div
      ref={containerRef}
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
      style={{
        position: 'fixed',
        inset: 0,
        width: '100vw',
        height: '100vh',
        overflow: 'hidden',
        cursor: 'crosshair',
        background: '#000',
        userSelect: 'none',
      }}
    >
      {preview && (
        <img
          src={preview}
          alt=""
          draggable={false}
          style={{
            position: 'absolute',
            inset: 0,
            width: '100%',
            height: '100%',
            objectFit: 'fill',
            pointerEvents: 'none',
          }}
        />
      )}
      {/* Dims the whole desktop preview; the selection rectangle below sits
          on top with just a bright border to mark the chosen region. */}
      <div
        style={{
          position: 'absolute',
          inset: 0,
          background: 'rgba(0,0,0,0.45)',
          pointerEvents: 'none',
        }}
      />
      {selection && (
        <div
          style={{
            position: 'absolute',
            left: selection.left,
            top: selection.top,
            width: selection.width,
            height: selection.height,
            border: '1px solid #fff',
            boxShadow: '0 0 0 1px rgba(0,0,0,0.5)',
            background: 'rgba(255,255,255,0.08)',
            pointerEvents: 'none',
          }}
        />
      )}
      <div
        style={{
          position: 'absolute',
          top: 16,
          left: '50%',
          transform: 'translateX(-50%)',
          color: '#fff',
          font: '13px system-ui, sans-serif',
          background: 'rgba(0,0,0,0.5)',
          padding: '4px 12px',
          borderRadius: 6,
          pointerEvents: 'none',
        }}
      >
        Click and drag to select a region — Esc to cancel
      </div>
    </div>
  );
}
