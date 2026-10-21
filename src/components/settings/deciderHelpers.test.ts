import { describe, expect, it } from 'vitest';
import { modelChanged, parseThreshold, statLine } from './deciderHelpers';
import type { DeciderSettings } from '@/lib/types';

const base: DeciderSettings = {
  dialect: 'systemone',
  url: 'http://box:8080',
  model: 'd1-3B-Q8_0.gguf',
  features: {
    tool_gating: true,
    ingest_gate: true,
    page_classify: true,
    shell_screen: true,
  },
  thresholds: {},
};

describe('decision model helpers', () => {
  it('detects a swapped model only when both sides are known', () => {
    expect(modelChanged(base, 'd1-3B-Q8_0.gguf')).toBe(false);
    expect(modelChanged(base, 'other.gguf')).toBe(true);
    expect(modelChanged({ ...base, model: null }, 'other.gguf')).toBe(false);
    expect(modelChanged(base, null)).toBe(false);
  });

  it('parses threshold fields as probabilities', () => {
    expect(parseThreshold('')).toBeNull();
    expect(parseThreshold('0.4')).toBe(0.4);
    expect(parseThreshold('1.2')).toBeUndefined();
    expect(parseThreshold('abc')).toBeUndefined();
  });

  it('reports engine counters and stays quiet for plugin-side checks', () => {
    const s = { calls: 10, answered: 9, failed: 1, acted: 4 };
    expect(statLine('ingest_gate', s)).toContain('10 asked');
    expect(statLine('ingest_gate', s)).toContain('1 fell back');
    expect(statLine('ingest_gate', { ...s, calls: 0 })).toBeNull();
    expect(statLine('page_classify', s)).toBeNull();
  });
});
