import React from 'react';
import { createRoot } from 'react-dom/client';
import { windowReady } from '@/lib/ipc';
import { App } from './App';

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);

void windowReady();
