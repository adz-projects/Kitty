import { describe, expect, it } from 'vitest';
import { effortLabel, effortSummaryText, selectedEffort } from './effort';
import type { EffortSummary, ThinkingEffort } from './types';

const effort: ThinkingEffort = {
  current_value: 'medium',
  max_stars: 3,
  can_disable: true,
  options: [
    { value: 'off', name: 'Off', stars: 0 },
    { value: 'low', name: 'Low', stars: 1 },
    { value: 'medium', name: 'Medium', stars: 2 },
    { value: 'high', name: 'High', stars: 3 },
  ],
};

describe('effort picker helpers', () => {
  it('selects the current value, else the first option', () => {
    expect(selectedEffort(effort)?.stars).toBe(2);
    expect(selectedEffort({ ...effort, current_value: 'xhigh' })?.value).toBe('off');
    expect(selectedEffort({ ...effort, options: [] })).toBeNull();
  });

  it('labels levels by name and position', () => {
    expect(effortLabel(effort.options[0], 3)).toBe('Reasoning: Off');
    expect(effortLabel(effort.options[3], 3)).toBe('Reasoning: High (3 of 3)');
    // A toggle-only model has no "of N" to speak of.
    expect(effortLabel({ value: 'on', name: 'On', stars: 1 }, 1)).toBe('Reasoning: On');
  });
});

describe('effortSummaryText', () => {
  const base: EffortSummary = {
    levels: ['low', 'medium', 'high', 'xhigh', 'max'],
    can_disable: true,
    source: 'open_router',
    source_label: 'OpenRouter',
    detected_at: 1_000_000,
    detecting: false,
  };
  const now = 1_000_000 + 12 * 86400;

  it('describes a graded ladder with its source and age', () => {
    expect(effortSummaryText(base, now)).toBe(
      'Reasoning: Off + 5 levels (low → max) · via OpenRouter · 12 days ago'
    );
    expect(effortSummaryText({ ...base, can_disable: false }, now)).toContain(
      'Reasoning: 5 levels'
    );
  });

  it('covers toggles, no control and undetected cards', () => {
    expect(effortSummaryText({ ...base, levels: ['on'] }, now)).toMatch(/^Reasoning: Off \/ On/);
    expect(effortSummaryText({ ...base, levels: [], can_disable: false }, now)).toMatch(
      /^No reasoning control/
    );
    expect(effortSummaryText({ ...base, detected_at: null }, now)).toBe(
      'Reasoning levels not detected yet'
    );
    expect(effortSummaryText({ ...base, detected_at: null, detecting: true }, now)).toBe(
      'Detecting reasoning levels…'
    );
    expect(effortSummaryText({ ...base, detected_at: now - 30 }, now)).toMatch(/just now$/);
  });
});
