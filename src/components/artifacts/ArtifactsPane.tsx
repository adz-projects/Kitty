import { useCallback, useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { settle } from '@/lib/gesture';
import { registerEscapeLayer } from '@/lib/escapeStack';
import { describeOutput, outputsDir } from '@/lib/outputs';
import { usePanGesture } from '@/hooks/usePanGesture';
import { useBackDismiss } from '@/hooks/useBackDismiss';
import { useChatStore } from '@/stores/chatStore';
import { listingFor, useArtifactsStore } from '@/stores/artifactsStore';
import { useRouteStore } from '@/stores/routeStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';
import { FolderIcon } from '@/components/icons/FolderIcon';
import type { FileEntry } from '@/lib/types';
import { useContextMenu } from '@/components/shared/ContextMenu';
import { fileMenu } from '@/lib/contextMenu';

const POLL_INTERVAL_MS = 5000;
/** How often the folder is rescanned while no turn is running. */
const IDLE_POLL_INTERVAL_MS = 30000;

const NO_ENTRIES: FileEntry[] = [];

/** The artifacts drawer: what is in this chat's `outputs` folder — the files
    and folders the user asked the model to make, one row per top-level entry
    (a folder of fifty PDFs is one row). Listed from disk, so it also shows what
    was dropped in or removed from Explorer. Persists nothing app-side.

    It exists only while open — it never opens on its own — and is polled only
    then: every tick while a turn runs (that is when files appear), otherwise
    every IDLE_POLL_INTERVAL_MS and whenever the window regains focus.

    Desktop: a drawer over the window's right edge (absolutely positioned, so
    the conversation never moves); closes by the title-bar button, ✕ or Escape.
    Android: a bottom sheet that slides up on mount and closes by ✕, the scrim,
    Back, or a swipe down. Every one of those animates out first and only then
    calls `onClose`, which unmounts it. A swipe on the list only moves the sheet
    once the list is scrolled to its top — otherwise it scrolls the list, the
    same rule every Android bottom sheet follows. */
export function ArtifactsPane({ onClose }: { onClose: () => void }) {
  const cwd = useChatStore((s) => s.cwd);
  const entries = useArtifactsStore((s) => listingFor(s, cwd)) ?? NO_ENTRIES;
  const refresh = useArtifactsStore((s) => s.refresh);
  // The hub keeps the whole chat workspace mounted (hidden) while the user is
  // on the Settings/Wizard route — the poll must not keep scanning disk for a
  // pane nobody can see. (`document.hidden` alone only covers minimization.)
  const view = useRouteStore((s) => s.view);
  const android = isAndroid();

  useEffect(() => {
    if (!cwd || view !== 'chat') return;
    void refresh(cwd);
    let lastScan = Date.now();
    const scan = () => {
      lastScan = Date.now();
      void refresh(cwd);
    };
    const id = setInterval(() => {
      // Skip the round-trip while the window is minimized/backgrounded —
      // nothing's watching, and it just spends IPC + disk I/O for no reason.
      if (document.hidden) return;
      const busy = useChatStore.getState().busy;
      if (!busy && Date.now() - lastScan < IDLE_POLL_INTERVAL_MS) return;
      scan();
    }, POLL_INTERVAL_MS);
    window.addEventListener('focus', scan);
    return () => {
      clearInterval(id);
      window.removeEventListener('focus', scan);
    };
  }, [cwd, view, refresh]);

  const sheet = android;
  const sheetRef = useRef<HTMLElement>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const [closing, setClosing] = useState(false);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  // Escape closes the drawer, like any other dismissable layer.
  useEffect(() => {
    if (android) return;
    return registerEscapeLayer(() => onCloseRef.current());
  }, [android]);

  // Animate out, then unmount. The timeout backs up `transitionend`, which
  // never fires if the transition is skipped (reduced motion, or the sheet
  // was already at its closed position).
  const requestClose = useCallback(() => {
    const el = sheetRef.current;
    if (!el || window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
      onCloseRef.current();
      return;
    }
    // Class first, synchronously: clearing a mid-drag inline transform before
    // React re-renders with `closing` would paint one frame of the sheet
    // snapped back to fully open.
    el.classList.add('closing');
    el.style.transition = '';
    el.style.transform = '';
    setClosing(true);
  }, []);
  useEffect(() => {
    if (!closing) return;
    const el = sheetRef.current;
    let fired = false;
    const done = () => {
      if (fired) return;
      fired = true;
      onCloseRef.current();
    };
    // `transitionend` bubbles, so a child's own hover transition finishing
    // must not count as the sheet having left the screen.
    const onEnd = (e: TransitionEvent) => {
      if (e.target === el) done();
    };
    el?.addEventListener('transitionend', onEnd);
    const t = setTimeout(done, 350);
    return () => {
      el?.removeEventListener('transitionend', onEnd);
      clearTimeout(t);
    };
  }, [closing]);

  useBackDismiss(sheet && !closing, requestClose);

  usePanGesture(sheetRef, {
    axis: 'y',
    enabled: sheet && !closing,
    shouldStart: ({ target, dy }) => {
      if (dy <= 0) return false;
      const body = bodyRef.current;
      return !(body && body.contains(target) && body.scrollTop > 0);
    },
    onMove: (delta) => {
      const el = sheetRef.current;
      if (!el) return;
      el.style.transition = 'none';
      el.style.transform = `translateY(${Math.max(0, delta)}px)`;
    },
    onEnd: (delta, velocity) => {
      const el = sheetRef.current;
      if (!el) return;
      // `settle` measures toward open, and down is toward closed.
      if (settle('open', -delta, -velocity, el.offsetHeight) === 'closed') {
        requestClose();
      } else {
        el.style.transition = '';
        el.style.transform = '';
      }
    },
  });

  const openFolder = async () => {
    if (!cwd) return;
    try {
      await ipc.openPath(outputsDir(cwd));
    } catch {
      // No outputs folder yet: the chat's own folder is the next best place.
      void ipc.openPath(cwd);
    }
  };

  const lists =
    entries.length === 0 ? (
      <p className="muted" style={{ fontSize: 14, padding: '4px 8px' }}>
        Files you ask Kitty to make will appear here.
      </p>
    ) : (
      <div className="artifacts-list">
        {entries.map((e) => (
          <ArtifactCard key={e.path} entry={e} android={android} />
        ))}
      </div>
    );

  return (
    <>
      {sheet && (
        <div className={`sheet-scrim${closing ? ' closing' : ''}`} onClick={requestClose} />
      )}
      <aside
        ref={sheetRef}
        className={`artifacts-pane${sheet ? ' artifacts-sheet' : ''}${closing ? ' closing' : ''}`}
      >
        {sheet && <div className="sheet-handle" aria-hidden="true" />}
        <div className="artifacts-head">
          <span className="artifacts-title">
            Artifacts <span className="artifacts-count">{entries.length}</span>
          </span>
          {/* Android has no folder to open: the chat folder lives in the app's
              private data directory, which neither Explorer's equivalent nor
              any other app can see into. Files leave via "Download" instead. */}
          {cwd && !android && (
            <button
              className="link"
              title="Open this chat's outputs folder in Explorer"
              onClick={() => void openFolder()}
            >
              Open folder
            </button>
          )}
          <button
            className="artifacts-close"
            onClick={sheet ? requestClose : onClose}
            aria-label="Close artifacts"
          >
            ✕
          </button>
        </div>
        {/* Its own scroller on the sheet, so the handle and header stay put
            and a swipe down can tell "list at top" from "list scrolled". On
            desktop it's a plain wrapper and the pane scrolls as before. */}
        <div ref={bodyRef} className="artifacts-body">
          {lists}
        </div>
      </aside>
    </>
  );
}

function ArtifactCard({ entry, android }: { entry: FileEntry; android: boolean }) {
  const [error, setError] = useState<string | null>(null);
  // Every action reports failure (#59): a file moved or deleted since, a
  // clipboard that refused.
  const act = async (p: Promise<unknown>, what: string) => {
    try {
      await p;
      setError(null);
    } catch (e) {
      setError(`Couldn't ${what}: ${String(e)}`);
    }
  };
  const description = describeOutput({ name: entry.name, isDir: entry.is_dir, size: entry.size });
  const cardRef = useRef<HTMLDivElement>(null);
  useContextMenu(cardRef, () =>
    android
      ? null
      : fileMenu({
          open: () => void act(ipc.openPath(entry.path), 'open it'),
          reveal: () => void act(ipc.revealPath(entry.path), 'show it'),
          copyPath: () => void act(navigator.clipboard.writeText(entry.path), 'copy its path'),
        })
  );

  return (
    <div className="artifact-card" ref={cardRef}>
      <div className="artifact-name" title={entry.path}>
        {entry.is_dir ? <FolderIcon /> : <DocumentIcon />} <span>{entry.name}</span>
      </div>
      <div className="artifact-origin muted">{description}</div>
      {/* The full path is Windows-useful and Android-noise: there, it points
          into an app-private directory the user can't navigate to anyway. */}
      {!android && (
        // Cut from the front, not the back, so what stays visible is the end
        // of the path — the part that tells one file from another. The
        // leading mark keeps the drive letter and slashes in reading order
        // inside the right-to-left box that does the cutting.
        <div className="artifact-path muted" title={entry.path}>
          {`‎${entry.path}`}
        </div>
      )}
      <div className="artifact-actions">
        {android ? (
          // "Open"/"Show in Folder"/"Copy path" are all meaningless against an
          // app-private path. Saving a copy out through the system file picker
          // is the only way a file the model wrote reaches the user's device.
          !entry.is_dir && (
            <button
              onClick={() =>
                void ipc
                  .downloadFile(entry.path)
                  .then(() => setError(null))
                  .catch((e) => setError(String(e)))
              }
            >
              Download
            </button>
          )
        ) : (
          <>
            <button onClick={() => void act(ipc.openPath(entry.path), 'open it')}>Open</button>
            <button onClick={() => void act(ipc.revealPath(entry.path), 'show it')}>
              Show in Folder
            </button>
            <button
              onClick={() => void act(navigator.clipboard.writeText(entry.path), 'copy the path')}
            >
              Copy path
            </button>
          </>
        )}
      </div>
      {error && (
        <p className="muted" style={{ fontSize: 14 }} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
