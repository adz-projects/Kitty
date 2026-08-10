import React from 'react';
import { createRoot } from 'react-dom/client';
import '@/themes/base.css';
import { initTheme } from '@/lib/theme';
import { windowReady } from '@/lib/ipc';
import { App } from './App';

initTheme();

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);

void windowReady();
