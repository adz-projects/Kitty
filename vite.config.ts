import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { resolve } from 'node:path';

// Multi-page build: one HTML entry per Tauri window label.
// Paths are kept identical between dev (vite server) and prod (dist) so the
// Rust side can load `windows/<label>/index.html` uniformly (see windows.rs).

// One source of truth for the five window entries — mirrors windows.rs::url().
const WINDOWS = ['overlay', 'main', 'settings', 'wizard', 'screenshot-select'] as const;
const entryHtml = (w: string) => `src/windows/${w}/index.html`;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  resolve: {
    alias: { '@': resolve(__dirname, 'src') },
  },
  server: {
    port: 1420,
    strictPort: true,
    // Vite's watcher excludes only node_modules/.git/outDir by default, so
    // everything else under the repo root is watched. `plugins/` holds ~76k
    // gitignored build artifacts (two Rust `target/` trees, four Python
    // `.build-venv/` trees) and `src-tauri/` another ~32k — 84k entries vs.
    // 200 for the actual frontend. On Windows that many directory watches
    // starve the dev server's event loop long after it has bound the port,
    // which is what leaves every window blank. Nothing under either path is
    // imported by `src/` (comment references only).
    watch: { ignored: ['**/src-tauri/**', '**/plugins/**'] },
  },
  optimizeDeps: {
    // Pin the dep scanner to the real entries. Left unset, Vite globs
    // `**/*.html` from the repo root and treats stray files (pywin32 docs
    // under plugins/*/.build-venv, rustdoc output under src-tauri/target) as
    // optimizer entries.
    entries: WINDOWS.map(entryHtml),
  },
  build: {
    target: 'esnext',
    rollupOptions: {
      input: Object.fromEntries(WINDOWS.map((w) => [w, resolve(__dirname, entryHtml(w))])),
    },
  },
});
