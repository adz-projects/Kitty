import { describe, it, expect } from 'vitest';
import { isImageFileName } from './chatStore';

/** Backs the fix for chat-only mode dropping images into a useless text
    placeholder instead of the native ACP image-content-block path — this is
    the shared predicate `send()`'s agentic-mode image split and chat-only
    mode's `inlineFileAsAttachment` both now use, so they can't drift apart. */

describe('isImageFileName', () => {
  it('recognizes common image extensions case-insensitively', () => {
    for (const name of ['photo.png', 'PHOTO.PNG', 'a.jpg', 'a.jpeg', 'a.gif', 'a.webp', 'a.bmp']) {
      expect(isImageFileName(name)).toBe(true);
    }
  });

  it('rejects non-image extensions', () => {
    for (const name of ['report.pdf', 'notes.docx', 'data.csv', 'readme.txt', 'archive.zip']) {
      expect(isImageFileName(name)).toBe(false);
    }
  });

  it('rejects a name with no extension', () => {
    expect(isImageFileName('Makefile')).toBe(false);
  });
});
