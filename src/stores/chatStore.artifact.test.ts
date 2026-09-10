import { describe, it, expect } from 'vitest';
import { deriveArtifact, isArtifactInScope } from './chatStore';
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

/** The Artifacts pane is a view of the user's files, not of everything a tool
    touched. `isArtifactInScope` is what keeps `kitty-tools`' extract-once
    document cache and `kitty-web`'s downloads — both under
    `~/.cache/lean-goose-mcp` — out of it, along with anything staged in the
    OS temp directory. */
describe('isArtifactInScope', () => {
  const CHAT = 'C:/Users/me/Documents/Kitty/chats/abc';
  const WORK = 'D:/Projects/report';
  const ATTACHED = 'C:/Users/me/Downloads/handed-over.pdf';
  const scope = [CHAT, WORK, ATTACHED];

  it('keeps a file written into the chat home directory', () => {
    expect(isArtifactInScope(`${CHAT}/out.docx`, scope)).toBe(true);
    expect(isArtifactInScope(`${CHAT}/sub/deep/out.csv`, scope)).toBe(true);
  });

  it('keeps a file written into the selected working directory', () => {
    expect(isArtifactInScope(`${WORK}/summary.md`, scope)).toBe(true);
  });

  it('keeps an attached file, which is granted by exact path, not as a folder', () => {
    expect(isArtifactInScope(ATTACHED, scope)).toBe(true);
    // ...and does not turn that grant into a grant over its whole folder.
    expect(isArtifactInScope('C:/Users/me/Downloads/something-else.pdf', scope)).toBe(false);
  });

  it('drops the kitty-tools document cache (the reported bug)', () => {
    expect(isArtifactInScope('C:/Users/me/.cache/lean-goose-mcp/documents/9f2a.json', scope)).toBe(
      false
    );
  });

  it('drops a scraped page cached by kitty-web', () => {
    expect(isArtifactInScope('C:/Users/me/.cache/lean-goose-mcp/page.md', scope)).toBe(false);
  });

  it('drops anything staged in the OS temp directory', () => {
    expect(isArtifactInScope('C:/Users/me/AppData/Local/Temp/kt-docstore-1/x.txt', scope)).toBe(
      false
    );
  });

  it('is case- and separator-insensitive, as Windows paths require', () => {
    const backslashed = String.raw`C:\Users\me\Documents\Kitty\chats\ABC\Out.DOCX`;
    expect(isArtifactInScope(backslashed, scope)).toBe(true);
  });

  it('does not let a sibling directory pass on a shared prefix', () => {
    expect(isArtifactInScope(`${CHAT}-other/out.docx`, scope)).toBe(false);
  });

  it('keeps everything when the scope is not yet known, rather than swallowing real output', () => {
    expect(isArtifactInScope('C:/anywhere/out.docx', [])).toBe(true);
    expect(isArtifactInScope('C:/anywhere/out.docx', [null, ''])).toBe(true);
  });
});
