import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { resolve } from 'node:path';

// Multi-page build: one HTML entry per Tauri window label.
// Paths are kept identical between dev (vite server) and prod (dist) so the
// Rust side can load `windows/<label>/index.html` uniformly (see windows.rs).

// One source of truth for the window entries — mirrors windows.rs::url().
//
// Three, not five: `settings` and `wizard` became routes inside `hub`
// (docs/ANDROID.md §8.1), which is also what the Android shell mounts.
// `screenshot-select` stays its own entry despite §8.1 suggesting otherwise —
// it needs a decorationless, transparent, always-on-top window sized to one
// monitor, which a route inside a normal window cannot be.
const WINDOWS = ['overlay', 'hub', 'screenshot-select'] as const;

/** Set by `cargo tauri android dev` to the address it configured the device
    to load from. Absent for desktop dev, which is what keeps the server on
    loopback there. */
const TAURI_DEV_HOST = process.env.TAURI_DEV_HOST;
const entryHtml = (w: string) => `src/windows/${w}/index.html`;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  resolve: {
    alias: { '@': resolve(__dirname, 'src') },
  },
  server: {
    // Mobile dev needs the server reachable from the *phone*, not just from
    // this machine. `cargo tauri android dev` sets `TAURI_DEV_HOST` to the
    // LAN address it told the device to use and then waits for something to
    // answer there; bound to localhost (the default) it waits forever.
    //
    // Only set when that variable is present, so a plain `pnpm dev` on
    // desktop keeps binding loopback and does not put the dev server on the
    // local network as a side effect.
    host: TAURI_DEV_HOST || false,
    port: 1420,
    strictPort: true,
    // HMR needs an explicit host for the same reason — the websocket URL the
    // client dials is otherwise `localhost`, which on the phone is the phone.
    hmr: TAURI_DEV_HOST
      ? { protocol: 'ws', host: TAURI_DEV_HOST, port: 1421 }
      : undefined,
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
