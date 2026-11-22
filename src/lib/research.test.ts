import { describe, expect, it } from 'vitest';
import {
  clampTurns,
  coverageFraction,
  formatDuration,
  isStalled,
  milestoneKey,
  parseResearchCommand,
  phaseLabel,
  planFromArgs,
  planMarkdown,
  wavesFor,
} from './research';

describe('parseResearchCommand', () => {
  it('reads the brief and leaves the budget to the engine by default', () => {
    expect(parseResearchCommand('/deepresearch AI in SF, 1940-1970')).toEqual({
      brief: 'AI in SF, 1940-1970',
      maxTurns: null,
    });
  });

  it('takes a budget in any of its spellings, clamped', () => {
    expect(parseResearchCommand('/deepresearch turns=10 a dossier')).toEqual({
      brief: 'a dossier',
      maxTurns: 10,
    });
    expect(parseResearchCommand('/deepresearch turns: 6 x')?.maxTurns).toBe(6);
    expect(parseResearchCommand('/deepresearch --turns 99 x')?.maxTurns).toBe(20);
    expect(parseResearchCommand('/deepresearch turns=1 x')?.maxTurns).toBe(3);
  });

  it('is a whole-word command', () => {
    expect(parseResearchCommand('/deepresearcher x')).toBeNull();
    expect(parseResearchCommand('please /deepresearch x')).toBeNull();
    expect(parseResearchCommand('/deepresearch')).toEqual({ brief: '', maxTurns: null });
  });
});

describe('budgets', () => {
  it('keeps two turns for the critique and the write-up', () => {
    expect(wavesFor(4)).toBe(2);
    expect(wavesFor(3)).toBe(1);
    expect(clampTurns(Number.NaN)).toBe(4);
  });
});

describe('planMarkdown', () => {
  it('renders every part of a proposed plan', () => {
    const plan = planFromArgs({
      plan: {
        title: 'Thinking machines',
        objective: 'A bibliography',
        deliverable: { description: 'One entry per work', format: 'docx' },
        sections: [{ id: '1940s', title: 'The 1940s', description: 'First decade' }],
        coverage: [{ id: 'mags', label: 'Magazine fiction' }],
        questions: ['Which stories?'],
        boundaries: ['No film'],
        turns: 6,
      },
    });
    expect(plan).not.toBeNull();
    const md = planMarkdown(plan!);
    expect(md).toContain('### Thinking machines');
    expect(md).toContain('(Markdown and Word)');
    expect(md).toContain('6 turns: 4 research waves');
    expect(md).toContain('- The 1940s: First decade');
    expect(md).toContain('- Magazine fiction');
    expect(md).toContain('- No film');
    expect(md).not.toContain('Acceptance');
  });

  it('tolerates arguments that are not a plan', () => {
    expect(planFromArgs(null)).toBeNull();
    expect(planFromArgs({ title: 'x' })).toBeNull();
  });
});

describe('progress', () => {
  it('says where a run is', () => {
    expect(phaseLabel({ id: 'r', status: 'researching', waves: 2, turns_used: 1 })).toBe(
      'Wave 2 of 2'
    );
    expect(
      phaseLabel({ id: 'r', status: 'researching', waves: 3, step: { step: 'planning', wave: 1 } })
    ).toBe('Wave 1 of 3: planning');
    expect(phaseLabel({ id: 'r', status: 'writing', step: { step: 'verifying' } })).toBe(
      'Writing: checking citations'
    );
    expect(phaseLabel({ id: 'r', status: 'failed', error: 'no host' })).toBe('Failed: no host');
  });

  it('measures coverage by cells reached', () => {
    expect(
      coverageFraction({
        id: 'r',
        status: 'researching',
        coverage: [
          { id: 'a', label: 'A', findings: 2, corroborated: 1 },
          { id: 'b', label: 'B', findings: 0, corroborated: 0 },
        ],
      })
    ).toBe(0.5);
    expect(coverageFraction({ id: 'r', status: 'done' })).toBe(0);
  });
});

describe('liveness', () => {
  const now = 1_000_000;
  const live = {
    id: 'r',
    status: 'researching' as const,
    activity: { last_activity_ms: now - 30_000, stall_after_secs: 420 },
  };

  it('formats durations', () => {
    expect(formatDuration(12_400)).toBe('12s');
    expect(formatDuration(185_000)).toBe('3m 05s');
    expect(formatDuration(3_720_000)).toBe('1h 02m');
    expect(formatDuration(-5)).toBe('0s');
  });

  it('calls a run stuck only after its threshold of silence', () => {
    expect(isStalled(live, now)).toBe(false);
    expect(isStalled(live, now + 400_000)).toBe(true);
    expect(isStalled({ ...live, status: 'done' }, now + 400_000)).toBe(false);
    expect(isStalled({ ...live, status: 'planning' }, now + 400_000)).toBe(false);
    expect(isStalled({ id: 'r', status: 'researching' }, now)).toBe(false);
  });

  it('marks milestones once each', () => {
    expect(
      milestoneKey({ id: 'r', status: 'researching', step: { step: 'wave_done', wave: 2 } })
    ).toBe('r:wave:2');
    expect(milestoneKey({ id: 'r', status: 'writing', step: { step: 'critique_done' } })).toBe(
      'r:critique'
    );
    expect(milestoneKey({ id: 'r', status: 'researching', step: { step: 'planning' } })).toBeNull();
  });

  it('counts researchers reporting in', () => {
    expect(
      phaseLabel({
        id: 'r',
        status: 'researching',
        waves: 3,
        step: { step: 'researching', wave: 2, done: 4, of: 6 },
      })
    ).toBe('Wave 2 of 3: researching (4 of 6 reported)');
  });
});
