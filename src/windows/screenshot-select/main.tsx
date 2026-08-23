import React from 'react';
import { createRoot } from 'react-dom/client';
import { windowReady } from '@/lib/ipc';
import { App } from './App';

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
