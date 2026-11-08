import { describe, expect, it } from 'vitest';
import { describeReport, describeWhen, wakeTimerWarning } from './overnightHelpers';
import type { OvernightReport } from '@/lib/types';

const empty: OvernightReport = {
  unscreened_dropped: 0,
  captures_chunked: 0,
  candidates_added: 0,
  clusters_promoted: 0,
  chunks_promoted: 0,
  candidates_merged: 0,
  candidates_expired: 0,
  extraction_attempted: 0,
  extraction_succeeded: 0,
  extraction_failed: 0,
  propositions_written: 0,
  finished: true,
};

describe('overnight helpers', () => {
  it('describes a pass', () => {
    expect(describeReport(empty)).toBe('nothing new.');
    const r = { ...empty, candidates_added: 3, clusters_promoted: 1, propositions_written: 7 };
    expect(describeReport(r)).toBe(
      '3 new passages set aside, 1 recurring topic learned, 7 facts written.'
    );
    expect(describeReport({ ...r, finished: false })).toContain('resumes next time');
  });

  it('says when, relative to today', () => {
    const now = new Date(2026, 10, 7, 9, 0);
    expect(describeWhen(new Date(2026, 10, 7, 1, 12).toISOString(), now)).toMatch(/^today at /);
    expect(describeWhen(new Date(2026, 10, 6, 2, 40).toISOString(), now)).toMatch(/^yesterday at /);
    expect(describeWhen('garbage', now)).toBe('garbage');
  });

  it('warns only when wake timers are off on mains', () => {
    expect(wakeTimerWarning({ ac: false, dc: false })).toContain('Allow wake timers');
    expect(wakeTimerWarning({ ac: true, dc: false })).toBeNull();
    expect(wakeTimerWarning({ ac: null, dc: null })).toBeNull();
  });
});
