import { describe, expect, it } from 'vitest';
import type { ToolCallUpdate } from '@/lib/types';
import { isAutoSpecialistCollection, specialistToolTitle, type ToolCall } from './chatStore';

/** Backs the draft treatment for background specialists: when a model answers
    with reports still outstanding, the daemon collects them itself and the
    model answers again. What it said first is a draft, and only the daemon's
    own collection — marked `"auto": true` — may turn it into one. */
describe('isAutoSpecialistCollection', () => {
  const update = (title: string, rawInput: unknown) =>
    ({ toolCallId: 'x', title, rawInput }) as unknown as ToolCallUpdate;

  it('recognizes the collection the daemon made', () => {
    expect(
      isAutoSpecialistCollection(update('await_specialists', { wait: 'all', auto: true }))
    ).toBe(true);
  });

  it('leaves an await the model called on purpose alone', () => {
    expect(isAutoSpecialistCollection(update('await_specialists', { wait: 'any' }))).toBe(false);
  });

  it('leaves the mid-turn hand-over alone, auto though it is', () => {
    // `drain_ready_specialists` hands a finished report over the moment it
    // lands. The model had not answered yet, so the text before it is ordinary
    // working text, not a draft — greying it out mid-turn would be wrong.
    expect(
      isAutoSpecialistCollection(update('await_specialists', { wait: 'none', auto: true }))
    ).toBe(false);
  });

  it('ignores every other tool, even one carrying the same flag', () => {
    expect(isAutoSpecialistCollection(update('lean_file_read', { auto: true }))).toBe(false);
    expect(isAutoSpecialistCollection(update('await_specialists', null))).toBe(false);
  });
});

describe('specialistToolTitle', () => {
  const call = (title: string, input: unknown, output?: unknown): ToolCall => ({
    id: '1',
    title,
    status: 'completed',
    input,
    output,
  });

  it('names what a ticket started', () => {
    expect(
      specialistToolTitle(
        call('call_specialist', { specialist: 'researcher' }, '{"ok":true,"ticket":"sp-3"}')
      )
    ).toBe('Started researcher · sp-3');
  });

  it('counts collected reports', () => {
    expect(
      specialistToolTitle(call('await_specialists', {}, '{"ok":true,"reports":[{},{}]}'))
    ).toBe('Collected 2 reports');
    expect(specialistToolTitle(call('await_specialists', { auto: true }))).toBe(
      'Collecting specialist reports'
    );
  });

  it('keeps other tools untouched', () => {
    expect(specialistToolTitle(call('lean_file_read', {}))).toBeNull();
  });
});
