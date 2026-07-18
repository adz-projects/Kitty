import { describe, it, expect } from 'vitest';
import { hasRepetitionLoop } from './chatStore';

/** Backs the fix for a real, observed local-model failure mode: instead of
    ever finishing, the model got stuck repeating a short "planning out loud"
    block near-verbatim dozens of times, and the trailing portion of it landed
    outside the reasoning/thinking container in the visible answer bubble. */

const LOOP_BLOCK =
  "I'll use `write` to save `annotated_bib.yaml`.\nI'll use `decide` to set context.\n\nI'm ready.\n";

describe('hasRepetitionLoop', () => {
  it('detects a short block repeating many times in a row', () => {
    const text = LOOP_BLOCK.repeat(20);
    expect(hasRepetitionLoop(text)).toBe(true);
  });

  it('is false for normal prose of similar length', () => {
    const paragraphs = Array.from(
      { length: 20 },
      (_, i) =>
        `Paragraph ${i}: here is a distinct sentence explaining strategy number ${i} in some detail.`
    );
    const text = paragraphs.join('\n\n');
    expect(hasRepetitionLoop(text)).toBe(false);
  });

  it('is false for short text below the minimum length', () => {
    expect(hasRepetitionLoop(LOOP_BLOCK)).toBe(false);
  });

  it('is false for structured content with short repeated separators', () => {
    // A markdown table's repeated "| --- |" separators are much shorter than
    // the 100-char default chunk size, so they should never trip the guard.
    const rows = Array.from({ length: 30 }, (_, i) => `| item ${i} | value ${i} | note ${i} |`);
    const text = ['| Item | Value | Note |', '| --- | --- | --- |', ...rows].join('\n');
    expect(hasRepetitionLoop(text)).toBe(false);
  });

  it('only considers the trailing window, not stale repeats far in the past', () => {
    // Same block repeated early on, but not enough times within the trailing
    // window on its own, followed by plenty of non-repeating filler.
    const earlyLoop = LOOP_BLOCK.repeat(3);
    const filler = Array.from(
      { length: 40 },
      (_, i) => `Distinct filler sentence number ${i} describing something unrelated.`
    ).join('\n');
    const text = earlyLoop + filler;
    expect(hasRepetitionLoop(text, 500)).toBe(false);
  });
});
