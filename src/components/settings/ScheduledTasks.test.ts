import { describe, it, expect } from 'vitest';
import { secondsToAmountUnit, UNIT_SECONDS } from './ScheduledTasks';

/** Backs the interval reverse-mapping used when opening an existing
    recurring task's edit form — picks the largest whole unit that divides
    `interval_secs` evenly, since the form only ever writes back a clean
    amount+unit pair (never a raw seconds count). */

describe('secondsToAmountUnit', () => {
  it('prefers days when evenly divisible', () => {
    expect(secondsToAmountUnit(2 * UNIT_SECONDS.days)).toEqual({ amount: 2, unit: 'days' });
  });

  it('falls back to hours when not a whole number of days', () => {
    expect(secondsToAmountUnit(3 * UNIT_SECONDS.hours)).toEqual({ amount: 3, unit: 'hours' });
  });

  it('falls back to minutes when not a whole number of hours', () => {
    expect(secondsToAmountUnit(90 * UNIT_SECONDS.minutes)).toEqual({ amount: 90, unit: 'minutes' });
  });

  it('rounds an odd, non-unit-aligned value to the nearest minute rather than failing', () => {
    // 3661s = 1h 1m 1s — not evenly divisible by an hour or a day.
    expect(secondsToAmountUnit(3661)).toEqual({ amount: 61, unit: 'minutes' });
  });

  it('never returns an amount below 1, even for sub-minute input', () => {
    expect(secondsToAmountUnit(30).amount).toBeGreaterThanOrEqual(1);
  });

  it('treats exactly one day as 1 day, not 24 hours', () => {
    expect(secondsToAmountUnit(UNIT_SECONDS.days)).toEqual({ amount: 1, unit: 'days' });
  });
});
