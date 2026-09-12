import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';

import { useChatStore, applySubagentStatus, resetSubagents } from './chatStore';
import type { SubagentStatusEvent } from '@/lib/types';

/** The specialist tray used to be permanent: one chip per delegate, kept for
    the rest of the turn. A three-way fan-out therefore wrapped onto multiple
    lines on a phone and stayed there. Chips now clear themselves a few seconds
    after the delegate finishes — long enough that a failure registers, short
    enough that the tray is not furniture.
 *
 *  The timer is the part worth testing. It is per-delegate and module-level,
 *  so the two ways to get it wrong are stacking two deletions for one chip,
 *  and letting a timer armed in one turn fire into the next and delete a chip
 *  that is still live. */
const LINGER_MS = 5_000;

const evt = (
  child: string,
  status: SubagentStatusEvent['status'],
  session = 's1'
): SubagentStatusEvent => ({
  session_id: session,
  child_session_id: child,
  specialist: 'researcher',
  status,
});

describe('specialist chips linger, then disappear', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    useChatStore.setState({ subagents: resetSubagents() });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('keeps a running chip indefinitely', () => {
    applySubagentStatus(evt('c1', 'started'));
    vi.advanceTimersByTime(LINGER_MS * 10);
    expect(useChatStore.getState().subagents).toHaveLength(1);
  });

  it('keeps a finished chip briefly, then drops it', () => {
    applySubagentStatus(evt('c1', 'started'));
    applySubagentStatus(evt('c1', 'completed'));

    // Still visible immediately after finishing — this is the window in which
    // a failure has to be noticeable.
    expect(useChatStore.getState().subagents).toHaveLength(1);

    vi.advanceTimersByTime(LINGER_MS - 1);
    expect(useChatStore.getState().subagents).toHaveLength(1);

    vi.advanceTimersByTime(2);
    expect(useChatStore.getState().subagents).toHaveLength(0);
  });

  it('drops a failed chip on the same schedule as a completed one', () => {
    applySubagentStatus(evt('c1', 'failed'));
    vi.advanceTimersByTime(LINGER_MS + 1);
    expect(useChatStore.getState().subagents).toHaveLength(0);
  });

  it('reschedules rather than stacking when a second frame arrives', () => {
    applySubagentStatus(evt('c1', 'failed'));
    vi.advanceTimersByTime(LINGER_MS - 100);
    // A retry reports success for the same delegate; its chip must survive a
    // full linger from *now*, not die on the first frame's timer.
    applySubagentStatus(evt('c1', 'completed'));
    vi.advanceTimersByTime(200);
    expect(useChatStore.getState().subagents).toHaveLength(1);

    vi.advanceTimersByTime(LINGER_MS);
    expect(useChatStore.getState().subagents).toHaveLength(0);
  });

  it('a pending timer cannot delete a chip belonging to the next turn', () => {
    applySubagentStatus(evt('c1', 'completed'));
    // A fresh turn resets the tray; `resetSubagents` must cancel the timer.
    useChatStore.setState({ subagents: resetSubagents() });
    applySubagentStatus(evt('c2', 'started'));

    vi.advanceTimersByTime(LINGER_MS * 2);
    const left = useChatStore.getState().subagents;
    expect(left).toHaveLength(1);
    expect(left[0].child_session_id).toBe('c2');
  });
});
