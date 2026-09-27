import { describe, expect, it } from 'vitest';
import { MAX_PASTED_TEXT_BYTES, pastedFileKind } from './pasteFiles';

describe('pastedFileKind', () => {
  it('sorts pasted files into what they can become', () => {
    expect(pastedFileKind({ name: 'shot.png', type: 'image/png', size: 10 })).toBe('image');
    expect(pastedFileKind({ name: 'notes.md', type: '', size: 10 })).toBe('text');
    expect(pastedFileKind({ name: 'a.txt', type: 'text/plain', size: 10 })).toBe('text');
    expect(
      pastedFileKind({ name: 'big.csv', type: 'text/csv', size: MAX_PASTED_TEXT_BYTES + 1 })
    ).toBe('too_large');
    expect(pastedFileKind({ name: 'report.docx', type: '', size: 10 })).toBe('unsupported');
  });
});
