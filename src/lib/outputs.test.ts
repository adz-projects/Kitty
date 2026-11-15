import { describe, it, expect } from 'vitest';
import {
  changedEntries,
  describeOutput,
  fileTypeLabel,
  formatBytes,
  outputEntryOf,
  outputsDir,
  sortEntries,
} from './outputs';
import type { FileEntry } from './types';

const entry = (name: string, over: Partial<FileEntry> = {}): FileEntry => ({
  name,
  path: `C:/chats/a/outputs/${name}`,
  size: 10,
  modified: 100,
  is_dir: false,
  ...over,
});

describe('outputsDir', () => {
  it('appends outputs, whatever the separators or trailing slash', () => {
    expect(outputsDir('C:/chats/a')).toBe('C:/chats/a/outputs');
    expect(outputsDir('C:\\chats\\a\\')).toBe('C:/chats/a/outputs');
  });
});

describe('outputEntryOf', () => {
  const cwd = 'C:/chats/a';

  it('maps a file directly in outputs to itself', () => {
    expect(outputEntryOf('C:/chats/a/outputs/report.docx', cwd)).toEqual({
      name: 'report.docx',
      path: 'C:/chats/a/outputs/report.docx',
      nested: false,
    });
  });

  it('maps a file in a subfolder to the top-level folder', () => {
    expect(outputEntryOf('C:/chats/a/outputs/pdfs/2024/one.pdf', cwd)).toEqual({
      name: 'pdfs',
      path: 'C:/chats/a/outputs/pdfs',
      nested: true,
    });
  });

  it('handles backslashes and is case-insensitive on Windows paths', () => {
    expect(outputEntryOf('c:\\Chats\\A\\OUTPUTS\\Sheet.xlsx', cwd)?.name).toBe('Sheet.xlsx');
  });

  it('is exact on a non-Windows path', () => {
    expect(outputEntryOf('/data/chat/outputs/a.txt', '/data/chat')?.name).toBe('a.txt');
    expect(outputEntryOf('/data/chat/OUTPUTS/a.txt', '/data/chat')).toBeNull();
  });

  it('is null outside the outputs directory', () => {
    expect(outputEntryOf('C:/chats/a/notes.md', cwd)).toBeNull();
    expect(outputEntryOf('C:/chats/a/scratch/outputs/x.md', cwd)).toBeNull();
    expect(outputEntryOf('C:/chats/b/outputs/x.md', cwd)).toBeNull();
  });

  it('is null for the outputs directory itself, and for a sibling sharing its prefix', () => {
    expect(outputEntryOf('C:/chats/a/outputs', cwd)).toBeNull();
    expect(outputEntryOf('C:/chats/a/outputs/', cwd)).toBeNull();
    expect(outputEntryOf('C:/chats/a/outputs-old/x.md', cwd)).toBeNull();
  });
});

describe('changedEntries', () => {
  it('returns new entries', () => {
    const before = [entry('a.txt')];
    const after = [entry('a.txt'), entry('b.txt')];
    expect(changedEntries(before, after).map((e) => e.name)).toEqual(['b.txt']);
  });

  it('returns an entry whose modified time or size changed', () => {
    const before = [entry('a.txt'), entry('b.txt'), entry('c.txt')];
    const after = [entry('a.txt'), entry('b.txt', { modified: 200 }), entry('c.txt', { size: 99 })];
    expect(changedEntries(before, after).map((e) => e.name)).toEqual(['b.txt', 'c.txt']);
  });

  it('ignores unchanged and removed entries', () => {
    expect(changedEntries([entry('a.txt'), entry('gone.txt')], [entry('a.txt')])).toEqual([]);
  });

  it('matches names case-insensitively', () => {
    expect(changedEntries([entry('Report.docx')], [entry('report.docx')])).toEqual([]);
  });

  it('treats everything as new when nothing was there before', () => {
    expect(changedEntries([], [entry('a'), entry('b')])).toHaveLength(2);
  });
});

describe('sortEntries', () => {
  it('puts the newest first, then by name, without mutating its input', () => {
    const input = [
      entry('b', { modified: 1 }),
      entry('a', { modified: 1 }),
      entry('c', { modified: 5 }),
    ];
    expect(sortEntries(input).map((e) => e.name)).toEqual(['c', 'a', 'b']);
    expect(input.map((e) => e.name)).toEqual(['b', 'a', 'c']);
  });
});

describe('fileTypeLabel', () => {
  it('names common types', () => {
    expect(fileTypeLabel('r.docx', false)).toBe('Word document');
    expect(fileTypeLabel('r.PDF', false)).toBe('PDF');
    expect(fileTypeLabel('x.py', false)).toBe('Python script');
    expect(fileTypeLabel('x.xlsx', false)).toBe('Excel workbook');
  });

  it('calls a folder a folder, whatever its name', () => {
    expect(fileTypeLabel('downloads', true)).toBe('Folder');
    expect(fileTypeLabel('v1.2', true)).toBe('Folder');
  });

  it('falls back to the extension, then to "File"', () => {
    expect(fileTypeLabel('data.parquet', false)).toBe('PARQUET file');
    expect(fileTypeLabel('Makefile', false)).toBe('File');
    expect(fileTypeLabel('.gitignore', false)).toBe('File');
    expect(fileTypeLabel('weird.', false)).toBe('File');
  });
});

describe('formatBytes / describeOutput', () => {
  it('formats sizes', () => {
    expect(formatBytes(12)).toBe('12 B');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(24 * 1024)).toBe('24 KB');
    expect(formatBytes(5 * 1024 * 1024)).toBe('5.0 MB');
    expect(formatBytes(-1)).toBe('');
  });

  it('adds the size when known, and omits it for a folder', () => {
    expect(describeOutput({ name: 'r.docx', size: 24 * 1024 })).toBe('Word document · 24 KB');
    expect(describeOutput({ name: 'r.docx' })).toBe('Word document');
    expect(describeOutput({ name: 'pdfs', isDir: true, size: 4096 })).toBe('Folder');
  });
});
