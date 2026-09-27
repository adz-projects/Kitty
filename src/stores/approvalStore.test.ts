import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PendingApproval } from '@/lib/types';

vi.mock('@/lib/ipc', () => ({
  ipc: { listPendingApprovals: vi.fn(), answerApproval: vi.fn() },
  onPendingApproval: vi.fn(() => Promise.resolve(() => {})),
  onApprovalResolved: vi.fn(() => Promise.resolve(() => {})),
}));

const { ipc } = await import('@/lib/ipc');
const { useApprovalStore, upsertApproval, offScreenApprovals } = await import('./approvalStore');

const approval = (id: string, session: string): PendingApproval => ({
  action_id: id,
  session_id: session,
  tool_name: 'shell',
  tool_args: { command: 'git push' },
  warning: null,
  always_scope: { args_pattern: null, label: 'git push …' },
  scheduled: false,
});

beforeEach(() => {
  vi.clearAllMocks();
  useApprovalStore.setState({ pending: [], answering: [], error: null });
});

describe('approval helpers', () => {
  it('upserts by action id', () => {
    const a = approval('a', 's1');
    expect(upsertApproval([a], { ...a, warning: 'w' })).toEqual([{ ...a, warning: 'w' }]);
    expect(upsertApproval([a], approval('b', 's1'))).toHaveLength(2);
  });

  it('puts only other chats in the dialog', () => {
    const list = [approval('a', 's1'), approval('b', 's2')];
    expect(offScreenApprovals(list, 's1').map((x) => x.action_id)).toEqual(['b']);
    expect(offScreenApprovals(list, null)).toHaveLength(2);
  });
});

describe('approvalStore.answer', () => {
  it('removes the approval once the answer lands', async () => {
    useApprovalStore.setState({ pending: [approval('a', 's1')] });
    vi.mocked(ipc.answerApproval).mockResolvedValue(undefined);

    expect(await useApprovalStore.getState().answer('a', 'allow')).toBe(true);
    expect(ipc.answerApproval).toHaveBeenCalledWith('a', 'allow');
    expect(useApprovalStore.getState().pending).toEqual([]);
  });

  it('keeps it to answer again when the answer fails', async () => {
    useApprovalStore.setState({ pending: [approval('a', 's1')] });
    vi.mocked(ipc.answerApproval).mockRejectedValue(new Error('boom'));

    expect(await useApprovalStore.getState().answer('a', 'reject')).toBe(false);
    const s = useApprovalStore.getState();
    expect(s.pending).toHaveLength(1);
    expect(s.answering).toEqual([]);
    expect(s.error).toBe('Error: boom');
  });

  it('sends one answer for a double click', async () => {
    useApprovalStore.setState({ pending: [approval('a', 's1')] });
    let finish!: () => void;
    vi.mocked(ipc.answerApproval).mockImplementation(
      () => new Promise<void>((res) => (finish = res))
    );
    const first = useApprovalStore.getState().answer('a', 'allow');
    expect(await useApprovalStore.getState().answer('a', 'allow')).toBe(false);
    finish();
    await first;
    expect(ipc.answerApproval).toHaveBeenCalledTimes(1);
  });
});
