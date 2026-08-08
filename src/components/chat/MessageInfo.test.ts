import { describe, it, expect } from 'vitest';
import { formatCacheHitRate } from './MessageInfo';
import type { Message } from '@/stores/chatStore';

function msg(partial: Partial<Message>): Message {
  return partial as Message;
}

describe('formatCacheHitRate', () => {
  it('computes hit rate as the cached share of total input tokens', () => {
    // inputTokens is normalized to *total* prompt size (see
    // usage_map_from_anthropic) — 890 of 1242 read from cache.
    const s = formatCacheHitRate(msg({ inputTokens: 1242, cacheReadTokens: 890 }));
    expect(s).toMatch(/^72% hit rate/);
    expect(s).toContain('890 read');
    expect(s).toContain('of 1242');
  });

  it('includes the written (cache-creation) count only when present', () => {
    const withCreation = formatCacheHitRate(
      msg({ inputTokens: 1242, cacheReadTokens: 890, cacheCreationTokens: 340 })
    );
    expect(withCreation).toContain('340 written');

    const withoutCreation = formatCacheHitRate(msg({ inputTokens: 1242, cacheReadTokens: 890 }));
    expect(withoutCreation).not.toContain('written');
  });

  it('reports 0% when nothing was read from cache', () => {
    const s = formatCacheHitRate(msg({ inputTokens: 500, cacheCreationTokens: 500 }));
    expect(s).toMatch(/^0% hit rate/);
  });

  it('does not fabricate a "of 0" hit rate when inputTokens is missing', () => {
    // Cache tokens can be reported without inputTokens (partial provider
    // report) — a "0% hit rate (… of 0)" line is meaningless, say n/a.
    const s = formatCacheHitRate(msg({ cacheReadTokens: 0 }));
    expect(s).not.toMatch(/0%/);
    expect(s).toMatch(/n\/a/);
    expect(formatCacheHitRate(msg({ cacheReadTokens: 540 }))).toMatch(/540 read \(n\/a total\)/);
  });
});
