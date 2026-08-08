import { describe, it, expect } from 'vitest';
import {
  countToolCall,
  toolCallSignature,
  trackToolAlternation,
  type ToolCallCounts,
} from './chatStore';

/** Backs the chat-mode tool-loop guard (owner-reported bug: a model got stuck
    alternating web_scrape/cache tool calls against the same target, each a
    real network/disk round-trip). */

describe('toolCallSignature', () => {
  it('uses the url field when present', () => {
    expect(toolCallSignature('web_scrape', { url: 'https://example.com/docs' })).toBe(
      'web_scrape::https://example.com/docs'
    );
  });

  it('falls back through path/file_path/paths/command in order', () => {
    expect(toolCallSignature('write', { path: 'a.txt' })).toBe('write::a.txt');
    expect(toolCallSignature('write', { file_path: 'b.txt' })).toBe('write::b.txt');
    expect(toolCallSignature('write', { paths: ['c.txt', 'd.txt'] })).toBe('write::c.txt');
    expect(toolCallSignature('shell', { command: 'ls' })).toBe('shell::ls');
  });

  it('distinguishes different targets for the same tool', () => {
    expect(toolCallSignature('web_scrape', { url: 'https://a.com' })).not.toBe(
      toolCallSignature('web_scrape', { url: 'https://b.com' })
    );
  });

  it('distinguishes different tools for the same target', () => {
    expect(toolCallSignature('web_scrape', { url: 'https://a.com' })).not.toBe(
      toolCallSignature('cache', { url: 'https://a.com' })
    );
  });

  it('falls back to a stable JSON representation when no known field matches', () => {
    const sig = toolCallSignature('mystery', { foo: 'bar' });
    expect(sig).toBe('mystery::{"foo":"bar"}');
  });
});

describe('countToolCall', () => {
  it('starts at 1 and increments per identical signature', () => {
    let counts: ToolCallCounts = new Map();
    let r = countToolCall(counts, 'web_scrape', { url: 'https://a.com' });
    expect(r.count).toBe(1);
    counts = r.counts;
    r = countToolCall(counts, 'web_scrape', { url: 'https://a.com' });
    expect(r.count).toBe(2);
    counts = r.counts;
    r = countToolCall(counts, 'web_scrape', { url: 'https://a.com' });
    expect(r.count).toBe(3);
  });

  it('tracks distinct signatures independently (the alternating-tools case)', () => {
    let counts: ToolCallCounts = new Map();
    for (let i = 0; i < 3; i++) {
      counts = countToolCall(counts, 'web_scrape', { url: 'https://a.com' }).counts;
      counts = countToolCall(counts, 'cache', { url: 'https://a.com' }).counts;
    }
    expect(countToolCall(counts, 'web_scrape', { url: 'https://a.com' }).count).toBe(4);
    expect(countToolCall(counts, 'cache', { url: 'https://a.com' }).count).toBe(4);
  });

  it('does not mutate the input counts map (pure)', () => {
    const counts: ToolCallCounts = new Map([['x', 1]]);
    countToolCall(counts, 'a', {});
    expect(counts.get('x')).toBe(1);
    expect(counts.size).toBe(1);
  });

  it('a different target resets the count for that signature', () => {
    let counts: ToolCallCounts = new Map();
    counts = countToolCall(counts, 'web_scrape', { url: 'https://a.com' }).counts;
    counts = countToolCall(counts, 'web_scrape', { url: 'https://a.com' }).counts;
    const r = countToolCall(counts, 'web_scrape', { url: 'https://different.com' });
    expect(r.count).toBe(1);
  });
});

describe('trackToolAlternation', () => {
  it('detects a sustained two-tool alternation against the same target', () => {
    let st = new Map<string, { lastTitle: string | null; flips: number }>();
    // A→B→A→B→A: each consecutive title change bumps flips.
    st = trackToolAlternation(st, 'web_scrape', { url: 'https://a.com' }).state;
    expect(trackToolAlternation(st, 'cache', { url: 'https://a.com' }).flips).toBe(1);
    st = trackToolAlternation(st, 'cache', { url: 'https://a.com' }).state;
    const r = trackToolAlternation(st, 'web_scrape', { url: 'https://a.com' });
    expect(r.flips).toBe(2);
  });

  it('sets flips back to 0 when the same tool repeats on a target', () => {
    let st = new Map<string, { lastTitle: string | null; flips: number }>();
    st = trackToolAlternation(st, 'web_scrape', { url: 'https://a.com' }).state;
    st = trackToolAlternation(st, 'cache', { url: 'https://a.com' }).state;
    st = trackToolAlternation(st, 'cache', { url: 'https://a.com' }).state;
    expect(trackToolAlternation(st, 'cache', { url: 'https://a.com' }).flips).toBe(0);
  });

  it('keys alternation by target, not by tool', () => {
    let st = new Map<string, { lastTitle: string | null; flips: number }>();
    st = trackToolAlternation(st, 'tool_a', { url: 'https://a.com' }).state;
    st = trackToolAlternation(st, 'tool_b', { url: 'https://a.com' }).state;
    // Different target starts its own sequence — this is the shared-key
    // behavior the loop guard relies on (bounces on a.com don't count
    // against b.com).
    const r = trackToolAlternation(st, 'tool_b', { url: 'https://b.com' });
    expect(r.flips).toBe(0);
  });

  it('returns 0 and unchanged state when no target can be extracted', () => {
    const st = new Map<string, { lastTitle: string | null; flips: number }>();
    const r = trackToolAlternation(st, 'mystery', { foo: 'bar' });
    expect(r.flips).toBe(0);
    expect(r.state.size).toBe(0);
  });

  it('does not mutate the input state (pure)', () => {
    const st = new Map<string, { lastTitle: string | null; flips: number }>([
      ['https://a.com', { lastTitle: 'cache', flips: 3 }],
    ]);
    trackToolAlternation(st, 'web_scrape', { url: 'https://a.com' });
    expect(st.get('https://a.com')).toEqual({ lastTitle: 'cache', flips: 3 });
  });
});
