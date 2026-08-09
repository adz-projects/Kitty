import { describe, it, expect } from 'vitest';
import { restartChipLabel, SAMPLING_PRESETS } from './LocalEngineSettings';

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
