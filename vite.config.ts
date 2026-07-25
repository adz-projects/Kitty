import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { resolve } from 'node:path';

// Multi-page build: one HTML entry per Tauri window label.
// Paths are kept identical between dev (vite server) and prod (dist) so the
// Rust side can load `windows/<label>/index.html` uniformly (see windows.rs).
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  resolve: {
    alias: { '@': resolve(__dirname, 'src') },
  },
  server: {
    port: 1420,
    strictPort: true,
    // Never watch the Rust build output (locked .dll/.exe cause EBUSY).
    watch: { ignored: ['**/src-tauri/**'] },
  },
  build: {
    target: 'esnext',
    rollupOptions: {
      input: {
        overlay: resolve(__dirname, 'src/windows/overlay/index.html'),
        main: resolve(__dirname, 'src/windows/main/index.html'),
        settings: resolve(__dirname, 'src/windows/settings/index.html'),
        wizard: resolve(__dirname, 'src/windows/wizard/index.html'),
        'screenshot-select': resolve(__dirname, 'src/windows/screenshot-select/index.html'),
      },
    },
  },
});
