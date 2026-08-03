import { describe, it, expect } from 'vitest';
import { parseArgs, formatArgs } from './McpServers';

/** Backs the fix for a real, reported bug: a plain `split(/\s+/)` tore a
    single Windows-path argument containing a space (e.g. anything under
    "...\Documents\Claude Code\...") into two separate array elements, so
    `node "<path>"` was actually invoked as `node "<first-half>" "<second-half>"`
    — Node silently failed to start, surfacing to the user as an opaque
    "No response from MCP server" with no indication of the real cause. */

describe('parseArgs', () => {
  it('splits plain unquoted args on whitespace, unchanged from before', () => {
    expect(parseArgs('--db-path foo.db')).toEqual(['--db-path', 'foo.db']);
  });

  it('keeps a double-quoted span with spaces as a single argument', () => {
    const path =
      'C:\\Users\\azolkover\\Documents\\Claude Code\\brave-search-mcp-server\\dist\\index.js';
    expect(parseArgs(`"${path}"`)).toEqual([path]);
  });

  it('mixes quoted and unquoted args in one string', () => {
    const path = 'C:\\Users\\azolkover\\Documents\\Claude Code\\dist\\index.js';
    expect(parseArgs(`"${path}" --transport stdio`)).toEqual([path, '--transport', 'stdio']);
  });

  it('collapses repeated whitespace between tokens', () => {
    expect(parseArgs('  --foo    bar  ')).toEqual(['--foo', 'bar']);
  });

  it('returns an empty array for blank input', () => {
    expect(parseArgs('')).toEqual([]);
    expect(parseArgs('   ')).toEqual([]);
  });

  it('reproduces the exact reported failure mode when NOT quoted', () => {
    // What actually happened before this fix: the space in "Claude Code"
    // split one path into two bogus args.
    const unquoted = 'C:\\Users\\azolkover\\Documents\\Claude Code\\dist\\index.js';
    expect(parseArgs(unquoted)).toEqual([
      'C:\\Users\\azolkover\\Documents\\Claude',
      'Code\\dist\\index.js',
    ]);
  });
});

describe('formatArgs', () => {
  it('leaves whitespace-free args unquoted', () => {
    expect(formatArgs(['--db-path', 'foo.db'])).toBe('--db-path foo.db');
  });

  it('re-quotes an arg containing whitespace', () => {
    const path = 'C:\\Users\\azolkover\\Documents\\Claude Code\\dist\\index.js';
    expect(formatArgs([path])).toBe(`"${path}"`);
  });

  it('round-trips through parseArgs without corruption', () => {
    const path = 'C:\\Users\\azolkover\\Documents\\Claude Code\\dist\\index.js';
    const original = [path, '--transport', 'stdio'];
    expect(parseArgs(formatArgs(original))).toEqual(original);
  });
});
