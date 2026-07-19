import { describe, it, expect } from 'vitest';
import { deriveArtifact } from './chatStore';
import type { ToolCallUpdate } from '@/lib/types';

/** Round-5: `deriveArtifact` gained a second qualifying signal (a recognized
    file extension on the output path) and an explicit read/view exclusion.
    These cover both the preserved behavior and the new cases. */

// Build a tool-call update the way goosed sends it: toolName lives under
// `_meta.goose.toolCall.toolName`; the path under `rawInput`.
function tc(toolName: string, rawInput: unknown, title = '', status?: string): ToolCallUpdate {
  return {
    toolCallId: 't1',
    title,
    rawInput,
    status,
    _meta: { goose: { toolCall: { toolName } } },
  } as ToolCallUpdate;
}

describe('deriveArtifact', () => {
  it('detects a text_editor write (existing behavior preserved)', () => {
    const a = deriveArtifact(tc('text_editor', { command: 'write', path: '/w/notes.md' }));
    expect(a).not.toBeNull();
    expect(a?.name).toBe('notes.md');
    expect(a?.path).toBe('/w/notes.md');
  });

  it('excludes an explicit view/read that carries a path (latent false-positive fix)', () => {
    // `text_editor` matches the write-verb regex by name, so without the read
    // exclusion a plain "view" would wrongly register as an artifact.
    expect(deriveArtifact(tc('text_editor', { command: 'view', path: '/w/notes.md' }))).toBeNull();
  });

  it('detects a spreadsheet by extension even when the tool name has no write verb', () => {
    const a = deriveArtifact(tc('make_report', { path: '/w/out/report.xlsx' }));
    expect(a).not.toBeNull();
    expect(a?.name).toBe('report.xlsx');
  });

  it('detects each owner-requested format by extension', () => {
    for (const f of ['data.csv', 'sheet.xlsx', 'doc.docx', 'readme.md', 'cfg.json', 'run.py']) {
      expect(deriveArtifact(tc('some_tool', { path: `/w/${f}` }))).not.toBeNull();
    }
  });

  it('ignores a non-write tool exposing a non-artifact extension', () => {
    // No write verb in the name, `.log` isn't a recognized artifact extension.
    expect(deriveArtifact(tc('tail_file', { path: '/w/server.log' }))).toBeNull();
  });

  it('ignores a shell command with no structured path (can not be detected)', () => {
    expect(deriveArtifact(tc('shell', { command: 'python make_xlsx.py' }))).toBeNull();
  });

  it('ignores a tool call with no path at all', () => {
    expect(deriveArtifact(tc('web_search', { query: 'hello' }))).toBeNull();
  });

  it('resolves a relative write path against the session cwd (fixes broken Open)', () => {
    const cwd = 'C:/Users/me/Documents/Kitty/chats/abc';
    const a = deriveArtifact(tc('write', { command: 'write', path: 'report.docx' }), cwd);
    expect(a?.path).toBe(`${cwd}/report.docx`);
    expect(a?.name).toBe('report.docx');
  });

  it('keeps an absolute path as-is even when a cwd is given', () => {
    const a = deriveArtifact(tc('write', { path: 'C:/other/out.csv' }), 'C:/Users/me/chats/abc');
    expect(a?.path).toBe('C:/other/out.csv');
  });

  it('never derives an artifact from a failed tool call (no file was actually produced)', () => {
    const a = deriveArtifact(
      tc('rag_ingest_file', { file_path: './report.docx' }, '', 'failed'),
      'C:/Users/me/chats/abc'
    );
    expect(a).toBeNull();
  });
});
