import { describe, it, expect } from 'vitest';
import { pathWithinDir, decideChatApproval, isGooseInternalCachePath } from './chatStore';

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

// Real approve-mode option set (docs/acp-protocol.md).
const OPTS = [
  { optionId: 'allow_always' },
  { optionId: 'allow_once' },
  { optionId: 'reject_once' },
  { optionId: 'reject_always' },
];

describe('decideChatApproval', () => {
  it('allows a write inside the chat folder (allow_once, no warning)', () => {
    const r = decideChatApproval({ command: 'write', path: `${BASE}/report.docx` }, BASE, OPTS);
    expect(r.optionId).toBe('allow_once');
    expect(r.warning).toBeUndefined();
  });

  it('rejects a write outside the chat folder with a warning', () => {
    const r = decideChatApproval({ path: 'C:/Users/me/Desktop/x.docx' }, BASE, OPTS);
    expect(r.optionId).toBe('reject_once');
    expect(r.warning).toMatch(/outside this chat's folder/);
  });

  it('allows a shell command (no structured path — the docx-export case)', () => {
    const r = decideChatApproval({ command: 'python make_docx.py' }, BASE, OPTS);
    expect(r.optionId).toBe('allow_once');
    expect(r.warning).toBeUndefined();
  });

  it('allows a relative in-folder path and honors file_path/paths variants', () => {
    expect(decideChatApproval({ path: 'out/report.xlsx' }, BASE, OPTS).optionId).toBe('allow_once');
    expect(decideChatApproval({ file_path: `${BASE}/a.md` }, BASE, OPTS).optionId).toBe(
      'allow_once'
    );
    expect(decideChatApproval({ paths: [`${BASE}/b.csv`] }, BASE, OPTS).optionId).toBe(
      'allow_once'
    );
  });

  it('allows when cwd is unknown (can not confine, so does not block)', () => {
    expect(decideChatApproval({ path: '/anywhere/x.py' }, null, OPTS).optionId).toBe('allow_once');
  });

  it('never picks allow_always (approval must not silently persist)', () => {
    const r = decideChatApproval({ command: 'shell' }, BASE, OPTS);
    expect(r.optionId).toBe('allow_once');
    expect(r.optionId).not.toBe('allow_always');
  });

  it("allows a write under Goose's own internal cache dir even though it is outside cwd", () => {
    const cache = 'C:/Users/me/AppData/Local/Block/goose/cache/computer_controller/web_1.txt';
    const r = decideChatApproval({ path: cache }, BASE, OPTS);
    expect(r.optionId).toBe('allow_once');
    expect(r.warning).toBeUndefined();
  });

  it('still rejects a real out-of-folder write that merely mentions "goose" elsewhere', () => {
    const r = decideChatApproval({ path: 'C:/Users/me/Desktop/goose-notes.txt' }, BASE, OPTS);
    expect(r.optionId).toBe('reject_once');
    expect(r.warning).toMatch(/outside this chat's folder/);
  });
});

describe('isGooseInternalCachePath', () => {
  it("matches Goose's cache dir regardless of slash style/case", () => {
    expect(
      isGooseInternalCachePath('C:\\Users\\me\\AppData\\Local\\Block\\goose\\cache\\x.txt')
    ).toBe(true);
    expect(isGooseInternalCachePath('c:/users/me/appdata/local/BLOCK/GOOSE/CACHE/y.txt')).toBe(
      true
    );
  });

  it('does not match unrelated paths', () => {
    expect(isGooseInternalCachePath('C:/Users/me/Desktop/goose-notes.txt')).toBe(false);
    expect(isGooseInternalCachePath('C:/Users/me/Documents/Kitty/chats/x/report.docx')).toBe(false);
  });
});
