import React from 'react';
import { createRoot } from 'react-dom/client';
import '@/themes/base.css';
import { initTheme } from '@/lib/theme';
import { windowReady, ipc } from '@/lib/ipc';
import { App } from './App';

initTheme();

// Report foreground/background to Rust so a turn that finishes while the user
// is in another app fires a system notification (and, on Android, so we can
// tell at all — there's no per-window focus there). `visibilitychange` fires
// when the user switches apps or tabs; seed the current state once at boot.
// Best-effort — a failed report just means the toast gate falls back to its
// default (foreground) assumption.
const reportForeground = () => void ipc.setAppForeground(!document.hidden).catch(() => {});
document.addEventListener('visibilitychange', reportForeground);
reportForeground();

// StrictMode double-invokes every render body and every mount effect, which
// is what surfaces the bugs it exists to surface — but in a shipped build it
// is a flat 2x on all render work, and the chat surface re-renders on every
// streamed frame. Dev keeps it; production does not.
createRoot(document.getElementById('root')!).render(
  import.meta.env.DEV ? (
    <React.StrictMode>
      <App />
    </React.StrictMode>
  ) : (
    <App />
  )
);

void windowReady();
