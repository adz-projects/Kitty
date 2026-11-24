import { describe, expect, it } from 'vitest';
import {
  chatsDeleted,
  groupCandidates,
  parseTidyCommand,
  proposalFromArgs,
  type TidyCandidate,
} from './tidy';
import { matchSlashCommands } from './slashCommands';

const cand = (ref: string, ids = [ref]): TidyCandidate => ({
  ref,
  session_ids: ids,
  title: ref,
  user_turns: 1,
  last_activity: null,
  files: 0,
  folder: null,
  background: ids.length - 1,
  runs: null,
  suggestion: 'delete',
  score: 60,
  reasons: ['1 turn'],
});

describe('/tidy', () => {
  it('parses the command and the wishes after it', () => {
    expect(parseTidyCommand('/tidy')).toBe('');
    expect(parseTidyCommand('  /tidy keep the op-ed ')).toBe('keep the op-ed');
    expect(parseTidyCommand('/tidyup')).toBeNull();
    expect(parseTidyCommand('please /tidy')).toBeNull();
  });

  it('reads a proposal defensively', () => {
    const p = proposalFromArgs({
      delete: ['c1', 2],
      unsure: ['c2'],
      reasons: { c2: ' title only ', c3: '' },
      summary: ' Mostly empty chats. ',
    });
    expect(p).toEqual({
      delete: ['c1'],
      unsure: ['c2'],
      reasons: { c2: 'title only' },
      summary: 'Mostly empty chats.',
    });
    expect(proposalFromArgs(null)).toEqual({ delete: [], unsure: [], reasons: {}, summary: null });
  });

  it('groups candidates; unlisted refs are kept and made-up refs ignored', () => {
    const g = groupCandidates([cand('c1'), cand('c2'), cand('c3')], {
      delete: ['c1', 'c2', 'c9'],
      unsure: ['c2'],
      reasons: { c1: 'empty' },
      summary: null,
    });
    expect(g.delete.map((r) => r.ref)).toEqual(['c1']);
    expect(g.delete[0].note).toBe('empty');
    expect(g.unsure.map((r) => r.ref)).toEqual(['c2']);
    expect(g.keep.map((r) => r.ref)).toEqual(['c3']);
  });

  it('counts background chats in what a delete removes', () => {
    const all = [cand('c1', ['a', 'a-bg1', 'a-bg2']), cand('c2')];
    expect(chatsDeleted(all, ['c1'])).toBe(3);
    expect(chatsDeleted(all, ['c1', 'c2'])).toBe(4);
    expect(chatsDeleted(all, [])).toBe(0);
  });

  it('is offered on desktop only', () => {
    expect(matchSlashCommands('/ti').map((c) => c.name)).toEqual(['tidy']);
    expect(matchSlashCommands('/ti', true)).toEqual([]);
  });
});
