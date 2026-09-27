import { describe, it, expect } from 'vitest';
import { pathWithinDir } from './chat/approvalUtils';

/** Backs the chat-mode "keep file ops inside the chat folder" soft boundary
    (Round-5). Windows path containment is fiddly (drive letters, case, `..`,
    sibling-prefix), so cover the cases that matter for the decision. */

const BASE = 'C:/Users/me/Documents/Kitty/chats/20260706_ab12';

describe('pathWithinDir', () => {
  it('accepts an absolute child path', () => {
    expect(pathWithinDir(BASE, `${BASE}/report.docx`)).toBe(true);
    expect(pathWithinDir(BASE, `${BASE}/sub/deep/x.csv`)).toBe(true);
  });

  it('accepts the folder itself', () => {
    expect(pathWithinDir(BASE, BASE)).toBe(true);
  });

  it('accepts a relative path (resolved against the folder)', () => {
    expect(pathWithinDir(BASE, 'report.docx')).toBe(true);
    expect(pathWithinDir(BASE, './out/report.xlsx')).toBe(true);
  });

  it('is case-insensitive and backslash-tolerant (Windows)', () => {
    expect(pathWithinDir(BASE, `${BASE.toUpperCase()}\\Report.DOCX`)).toBe(true);
    expect(pathWithinDir(BASE, `${BASE.replace(/\//g, '\\')}\\a\\b.py`)).toBe(true);
  });

  it('rejects an absolute path outside the folder', () => {
    expect(pathWithinDir(BASE, 'C:/Windows/System32/evil.dll')).toBe(false);
    expect(pathWithinDir(BASE, 'C:/Users/me/Documents/other.docx')).toBe(false);
  });

  it('rejects a sibling folder with a shared prefix', () => {
    // …/chats/20260706_ab12XX must NOT count as inside …/chats/20260706_ab12.
    expect(pathWithinDir(BASE, `${BASE}xx/report.docx`)).toBe(false);
  });

  it('rejects an escape via ..', () => {
    expect(pathWithinDir(BASE, `${BASE}/../../secrets.txt`)).toBe(false);
    expect(pathWithinDir(BASE, '../sibling/x.csv')).toBe(false);
  });

  it('rejects when there is no base', () => {
    expect(pathWithinDir('', `${BASE}/x`)).toBe(false);
  });
});
