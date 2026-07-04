import React from 'react';
import { createRoot } from 'react-dom/client';
import '@/themes/base.css';
import '@/themes/default.css';
import { App } from './App';

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
