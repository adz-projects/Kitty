import { describe, it, expect } from 'vitest';
import {
  offloadSummary,
  restartChipLabel,
  SAMPLING_PRESETS,
  vramSummary,
} from './LocalEngineSettings';
import type { LocalSlotStatus, SelectedBackend } from '@/lib/types';

/** A CPU device as the daemon reports it: no VRAM to show, but a real sizing
    budget. The two fields disagreeing here is the point — see
    `SelectedBackend.usable_memory`. */
const CPU_BACKEND: SelectedBackend = {
  backend: 'cpu',
  device: null,
  device_index: null,
  memory_free: 0,
  memory_total: 0,
  usable_memory: 16_000_000_000,
};

const slot = (over: Partial<LocalSlotStatus> = {}): LocalSlotStatus => ({
  kind: 'summarizer',
  loaded: true,
  backend: {
    backend: 'vulkan',
    device: 'AMD Radeon RX 7900',
    device_index: 0,
    memory_free: 8_000_000_000,
    memory_total: 24_000_000_000,
    usable_memory: 8_000_000_000,
  },
  n_gpu_layers: 16,
  n_ctx: 8192,
  ...over,
});

/** `n_gpu_layers: -1` ("automatic") can resolve to a full offload, a partial
    one, or none at all, and the three are not interchangeable to a user
    wondering why generation is slow. */
describe('offloadSummary', () => {
  it('names the device and the layer count for a GPU offload', () => {
    expect(offloadSummary(slot())).toBe('AMD Radeon RX 7900, 16 layers');
    expect(offloadSummary(slot(), 16)).toBe('AMD Radeon RX 7900, 16/16 layers');
  });

  /// The case the card exists for: fitting put some layers on the card and
  /// left the rest on the CPU. Reporting just "Vulkan" would hide it.
  it('shows a partial offload as a fraction rather than as "on the GPU"', () => {
    const s = offloadSummary(slot({ n_gpu_layers: 12 }), 16);
    expect(s).toContain('12/16');
  });

  it('distinguishes a GPU that is present from one that is being used', () => {
    expect(offloadSummary(slot({ n_gpu_layers: 0 }))).toMatch(/CPU/);
    expect(offloadSummary(slot({ n_gpu_layers: 0 }))).toMatch(/nothing offloaded/);
  });

  it('reports CPU plainly when there is no GPU backend at all', () => {
    expect(offloadSummary(slot({ backend: CPU_BACKEND, n_gpu_layers: 0 }))).toBe('CPU');
  });

  /// `-1` surviving into the status payload means fitting was skipped or
  /// failed and llama.cpp's own "all layers" default applied. That is a real
  /// state, not a missing value, so it must not render as "null layers".
  it('renders the all-layers sentinel as words, not as a number', () => {
    expect(offloadSummary(slot({ n_gpu_layers: -1 }))).toBe('AMD Radeon RX 7900, all layers');
    expect(offloadSummary(slot({ n_gpu_layers: null }))).toBe('AMD Radeon RX 7900, all layers');
  });

  it('says nothing about layers for an unloaded slot', () => {
    expect(offloadSummary(slot({ loaded: false }))).toBe('not loaded');
  });
});

/** CPU devices report 0/0, which must not render as a real budget — "0.0 /
    0.0 GB free" reads as a broken GPU rather than an absent one. */
describe('vramSummary', () => {
  it('formats a real VRAM budget', () => {
    expect(vramSummary(slot().backend)).toBe('8.0 / 24.0 GB free');
  });

  /// Note `CPU_BACKEND` carries 16 GB of `usable_memory`: the sizing budget
  /// is real, and the VRAM row still must not claim it.
  it('returns nothing when there is no VRAM budget to report', () => {
    expect(vramSummary(CPU_BACKEND)).toBeNull();
    expect(vramSummary(null)).toBeNull();
    expect(vramSummary(undefined)).toBeNull();
  });
});

/** §6.4 is explicit that a pending restart is a non-blocking chip, never a
    modal — and that it must distinguish "applying now" from "waiting for your
    reply to finish". A chip that said the same thing in both states would
    make the second case look like a hang. */
describe('restartChipLabel', () => {
  it('says nothing when no restart is outstanding', () => {
    expect(restartChipLabel({ reload_required: false, restart_pending: false })).toBeNull();
    // `restart_pending` without `reload_required` is not a state the Rust
    // side produces, but rendering a chip for it would be wrong regardless.
    expect(restartChipLabel({ reload_required: false, restart_pending: true })).toBeNull();
  });

  it('distinguishes an immediate restart from one queued behind a reply', () => {
    const now = restartChipLabel({ reload_required: true, restart_pending: false });
    const queued = restartChipLabel({ reload_required: true, restart_pending: true });
    expect(now).toBeTruthy();
    expect(queued).toBeTruthy();
    expect(now).not.toBe(queued);
    expect(queued).toMatch(/finish/i);
  });
});

/** These strings are resolved by name in
    `bigtiny_rust::provider::presets::resolve`, which returns `None` for
    anything it doesn't recognise — a typo here wouldn't error, it would
    silently apply no preset at all. Keep the two lists in step. */
describe('SAMPLING_PRESETS', () => {
  it('matches the daemon-side preset names exactly', () => {
    expect([...SAMPLING_PRESETS]).toEqual(['precise', 'balanced', 'creative']);
  });
});
