import { describe, it, expect } from 'vitest';
import {
  countToolCall,
  isIterativeEditTool,
  toolCallSignature,
  trackToolAlternation,
  TOOL_LOOP_THRESHOLD,
  type ToolAlternationState,
  type ToolCallCounts,
} from './loopGuards';

// The tool-loop guard exists to stop a model burning real network/disk I/O in
// a circle. These tests pin the line between that and ordinary iterative
// editing, which the guard used to declare a loop and auto-decline mid-task.

describe('isIterativeEditTool', () => {
  it('covers the write-class file tools and nothing else', () => {
    for (const t of [
      'lean_file_replace_str',
      'lean_file_replace_lines',
      'lean_file_append',
      'lean_file_write',
    ]) {
      expect(isIterativeEditTool(t)).toBe(true);
    }
    // Reads, searches and shell are still fully guarded — repeating those
    // against one target really is the stuck-loop signature.
    for (const t of ['lean_file_read', 'lean_web_scrape', 'lean_shell', 'lean_doc_search']) {
      expect(isIterativeEditTool(t)).toBe(false);
    }
  });

  it('sees through an MCP server namespace prefix', () => {
    // Defensive: BigTiny sends the bare name today, but a namespaced form
    // must not silently switch the exemption off.
    expect(isIterativeEditTool('kitty-tools__lean_file_replace_str')).toBe(true);
  });
});

describe('toolCallSignature', () => {
  it('distinguishes different edits to the same file', () => {
    // The bug: the signature was `tool::path`, so every edit to one file was
    // "the same call" and the fifth ordinary edit got declined.
    const a = toolCallSignature('lean_file_replace_str', {
      path: '/w/app.ts',
      old_str: 'foo',
      new_str: 'bar',
    });
    const b = toolCallSignature('lean_file_replace_str', {
      path: '/w/app.ts',
      old_str: 'baz',
      new_str: 'qux',
    });
    expect(a).not.toEqual(b);
  });

  it('still collapses a byte-identical edit repeated against the same file', () => {
    // The exemption is narrow on purpose: a model reissuing the *same* edit is
    // going in circles and must still be caught.
    const args = { path: '/w/app.ts', old_str: 'foo', new_str: 'bar' };
    expect(toolCallSignature('lean_file_replace_str', args)).toEqual(
      toolCallSignature('lean_file_replace_str', { ...args })
    );
  });

  it('discriminates line edits by their line range', () => {
    const a = toolCallSignature('lean_file_replace_lines', {
      path: '/w/app.ts',
      start_line: 10,
      end_line: 12,
      content: 'x',
    });
    const b = toolCallSignature('lean_file_replace_lines', {
      path: '/w/app.ts',
      start_line: 40,
      end_line: 42,
      content: 'x',
    });
    expect(a).not.toEqual(b);
  });

  it('leaves a non-edit tool keyed on its target alone', () => {
    const a = toolCallSignature('lean_web_scrape', { url: 'https://e.com', max_chars: 100 });
    const b = toolCallSignature('lean_web_scrape', { url: 'https://e.com', max_chars: 999 });
    expect(a).toEqual(b);
  });
});

describe('countToolCall with iterative edits', () => {
  it('does not trip the threshold across a long series of distinct edits', () => {
    // A ten-edit refactor of one file — previously declined at edit five.
    let counts: ToolCallCounts = new Map();
    let max = 0;
    for (let i = 0; i < 10; i++) {
      const r = countToolCall(counts, 'lean_file_replace_str', {
        path: '/w/app.ts',
        old_str: `old_${i}`,
        new_str: `new_${i}`,
      });
      counts = r.counts;
      max = Math.max(max, r.count);
    }
    expect(max).toBeLessThanOrEqual(TOOL_LOOP_THRESHOLD);
  });

  it('still trips on the same edit repeated', () => {
    let counts: ToolCallCounts = new Map();
    let last = 0;
    for (let i = 0; i < TOOL_LOOP_THRESHOLD + 2; i++) {
      const r = countToolCall(counts, 'lean_file_replace_str', {
        path: '/w/app.ts',
        old_str: 'foo',
        new_str: 'bar',
      });
      counts = r.counts;
      last = r.count;
    }
    expect(last).toBeGreaterThan(TOOL_LOOP_THRESHOLD);
  });
});

describe('trackToolAlternation with iterative edits', () => {
  it('does not count read/edit cycling on one file as alternation', () => {
    // read → edit → read → edit is how you edit a file correctly; it was a
    // perfect A→B→A→B flip sequence and tripped faster than the counter did.
    let state: ToolAlternationState = new Map();
    let max = 0;
    for (let i = 0; i < 8; i++) {
      for (const title of ['lean_file_read', 'lean_file_replace_str']) {
        const r = trackToolAlternation(state, title, { path: '/w/app.ts' });
        state = r.state;
        max = Math.max(max, r.flips);
      }
    }
    expect(max).toBe(0);
  });

  it('still catches two non-edit tools alternating on one target', () => {
    // The failure mode the guard was written for (web-fetch ↔ its cache step)
    // must be untouched by the exemption.
    let state: ToolAlternationState = new Map();
    let last = 0;
    for (let i = 0; i < 8; i++) {
      for (const title of ['lean_web_scrape', 'lean_cache_view']) {
        const r = trackToolAlternation(state, title, { url: 'https://e.com/a' });
        state = r.state;
        last = r.flips;
      }
    }
    expect(last).toBeGreaterThan(TOOL_LOOP_THRESHOLD);
  });
});
