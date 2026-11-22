import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRecallCard } from './MemoryRecallCard';

describe('MemoryRecallCard', () => {
  it('renders a tool-card summary with fact count, profile, and latency badge', () => {
    const html = renderToStaticMarkup(
      <MemoryRecallCard
        recall={{
          status: 'injected',
          duration_ms: 42,
          memorabilia: { status: 'injected', item_count: 3, text: '- [p_1] fact' },
          pathway: { status: 'seeded', text: 'prefers ArXiv preprints' },
        }}
      />
    );
    expect(html).toContain('class="tool-card"');
    expect(html).toContain('Memory recall · 3 facts, profile (thought seed)');
    expect(html).toContain('class="status-badge">injected · 42ms</span>');
  });

  it('renders empty and timed-out states cleanly', () => {
    const emptyHtml = renderToStaticMarkup(
      <MemoryRecallCard
        recall={{
          status: 'empty',
          duration_ms: 12,
          memorabilia: { status: 'empty' },
          pathway: { status: 'empty' },
        }}
      />
    );
    expect(emptyHtml).toContain('Memory recall');
    expect(emptyHtml).toContain('empty · 12ms');

    const timeoutHtml = renderToStaticMarkup(
      <MemoryRecallCard
        recall={{
          status: 'timed_out',
          duration_ms: 1500,
          memorabilia: { status: 'timed_out' },
        }}
      />
    );
    expect(timeoutHtml).toContain('timed out · 1500ms');
  });
});
