import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { MemorabiliaItem, PathwayBelief, PathwayStats } from '@/lib/types';

const mocks = vi.hoisted(() => ({
  getPathwayBeliefs: vi.fn(),
  deletePathwayBelief: vi.fn(),
  getPathwayStats: vi.fn(),
  getMemorabiliaItems: vi.fn(),
  deleteMemorabiliaItem: vi.fn(),
}));

vi.mock('@/lib/ipc', () => ({
  ipc: mocks,
}));

import { BeliefBrowser } from './BeliefBrowser';
import { GraphHealth } from './GraphHealth';
import { MemorabiliaBrowser } from './MemorabiliaBrowser';

async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe('Memory settings UI components', () => {
  let container: HTMLDivElement;

  beforeEach(() => {
    container = document.createElement('div');
    document.body.appendChild(container);
    vi.clearAllMocks();
  });

  afterEach(() => {
    container.remove();
  });

  it('GraphHealth displays staged synthesis and directive uptake stats', async () => {
    const stats: PathwayStats = {
      total: 12,
      tested: 7,
      contradicted: 1,
      avg_confidence: 0.78,
      by_layer: { identity: 3, context: 7, conversation: 2 },
      embedding_migration: { pending: 0, current_model: 'gemma' },
      learning: { last_learn_at: new Date().toISOString(), unlearned_sessions: 1 },
      staged: { pending: 4, sessions: 2 },
      signals: {
        challenge: { engaged: 3, total: 4 },
        fresh_angle: { engaged: 2, total: 5 },
      },
    };
    mocks.getPathwayStats.mockResolvedValue(stats);

    const root = createRoot(container);
    await act(async () => {
      root.render(<GraphHealth />);
    });
    await flush();

    expect(container.textContent).toContain('12');
    expect(container.textContent).toContain('78% avg confidence');
    expect(container.textContent).toContain('Across 2 chats');
    expect(container.textContent).toContain('Challenge: 3/4 (75%)');
    expect(container.textContent).toContain('Fresh angle: 2/5 (40%)');
    expect(mocks.getPathwayBeliefs).not.toHaveBeenCalled();

    act(() => root.unmount());
  });

  it('BeliefBrowser filters by layer/conflicts and forgets beliefs', async () => {
    const beliefs: PathwayBelief[] = [
      {
        id: 'b1',
        text: 'Relies on ArXiv preprints over journals',
        layer: 'identity',
        kind: 'preference',
        confidence: 0.85,
        tested: true,
        domain: 'research',
        support_count: 5,
        distinct_sessions: 3,
        contradict_count: 0,
        pinned: true,
      },
      {
        id: 'b2',
        text: 'Uses monolithic repos for small teams',
        layer: 'context',
        kind: 'stance',
        confidence: 0.6,
        tested: false,
        domain: 'coding',
        support_count: 2,
        distinct_sessions: 1,
        contradict_count: 1,
        pinned: false,
      },
    ];
    mocks.getPathwayBeliefs.mockResolvedValue({ beliefs });
    mocks.deletePathwayBelief.mockResolvedValue(undefined);
    const onMutated = vi.fn();

    const root = createRoot(container);
    await act(async () => {
      root.render(<BeliefBrowser onMutated={onMutated} />);
    });
    await flush();

    expect(container.querySelectorAll('.mem-item')).toHaveLength(2);

    const conflictsChip = Array.from(container.querySelectorAll('button')).find((b) =>
      b.textContent?.includes('Conflicts (1)')
    );
    expect(conflictsChip).toBeDefined();
    await act(async () => {
      conflictsChip!.click();
    });
    expect(container.querySelectorAll('.mem-item')).toHaveLength(1);
    expect(container.textContent).toContain('Uses monolithic repos');

    const forgetBtn = Array.from(container.querySelectorAll('button')).find(
      (b) => b.textContent === 'Forget'
    );
    await act(async () => {
      forgetBtn!.click();
    });
    await flush();

    expect(mocks.deletePathwayBelief).toHaveBeenCalledWith('b2');
    expect(onMutated).toHaveBeenCalledTimes(1);
    expect(container.querySelectorAll('.mem-item')).toHaveLength(0);

    act(() => root.unmount());
  });

  it('MemorabiliaBrowser filters by importance/disputed and forgets facts', async () => {
    const items: MemorabiliaItem[] = [
      {
        id: 'p1',
        claim: 'Memorabilia registers sqlite3_vec_init via auto_extension',
        confidence: 0.92,
        disputed: false,
        importance: 'high',
        urgency: 'none',
        created_at: '2026-11-20T00:00:00Z',
      },
      {
        id: 'p2',
        claim: 'Legacy config flag may conflict',
        confidence: 0.55,
        disputed: true,
        importance: 'medium',
        urgency: 'none',
        created_at: '2026-11-20T00:00:00Z',
      },
    ];
    mocks.getMemorabiliaItems.mockResolvedValue({ items });
    mocks.deleteMemorabiliaItem.mockResolvedValue(undefined);

    const root = createRoot(container);
    await act(async () => {
      root.render(<MemorabiliaBrowser />);
    });
    await flush();

    expect(container.querySelectorAll('.mem-item')).toHaveLength(2);

    const highChip = Array.from(container.querySelectorAll('button')).find(
      (b) => b.textContent === 'High importance'
    );
    await act(async () => {
      highChip!.click();
    });
    expect(container.querySelectorAll('.mem-item')).toHaveLength(1);
    expect(container.textContent).toContain('sqlite3_vec_init');

    const forgetBtn = Array.from(container.querySelectorAll('button')).find(
      (b) => b.textContent === 'Forget'
    );
    await act(async () => {
      forgetBtn!.click();
    });
    await flush();

    expect(mocks.deleteMemorabiliaItem).toHaveBeenCalledWith('p1');
    expect(container.querySelectorAll('.mem-item')).toHaveLength(0);

    act(() => root.unmount());
  });
});
