import { describe, it, expect } from 'vitest';
import { showSpecialistsToggle } from './SpecialistsToggle';

describe('showSpecialistsToggle', () => {
  it('shows on desktop while specialists are enabled in Settings', () => {
    expect(showSpecialistsToggle(false, true)).toBe(true);
  });

  it('hides when specialists are off in Settings', () => {
    expect(showSpecialistsToggle(false, false)).toBe(false);
  });

  it('hides on Android, which has no specialists', () => {
    expect(showSpecialistsToggle(true, true)).toBe(false);
    expect(showSpecialistsToggle(true, false)).toBe(false);
  });
});
