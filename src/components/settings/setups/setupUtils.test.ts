import { describe, it, expect } from 'vitest';
import type { ModelInfo, ProviderAccountView, Setup } from '@/lib/types';
import {
  blankSetup,
  checkedModels,
  duplicateOf,
  modelKey,
  moved,
  droppedAt,
  setupsUsing,
  splitModelKey,
} from './setupUtils';

function info(model: string, enabled: boolean): ModelInfo {
  return {
    provider_id: 'p',
    provider_name: 'P',
    provider_type: 'openrouter',
    model,
    card_id: null,
    name: model.toUpperCase(),
    enabled,
    vision: false,
    in_catalog: true,
    created: null,
    price_out_per_m: null,
    price_in_per_m: null,
    price_tier: null,
    intelligence: null,
    context_length: null,
  };
}

function account(id: string, infos: ModelInfo[], disabled = false): ProviderAccountView {
  return {
    id,
    name: id.toUpperCase(),
    model_infos: infos.map((m) => ({ ...m, provider_id: id })),
    disabled_reason: disabled ? 'unsupported' : null,
  } as unknown as ProviderAccountView;
}

const setup = (id: string, provider_id: string, model: string): Setup => ({
  id,
  name: `S${id}`,
  provider_id,
  model,
  created_at: '',
});

describe('setup helpers', () => {
  it('offers only checked models of usable providers, in rank order', () => {
    const groups = checkedModels([
      account('a', [info('x', true), info('y', false), info('z', true)]),
      account('b', [info('q', false)]),
      account('c', [info('w', true)], true),
    ]);
    expect(groups.map((g) => g.providerId)).toEqual(['a']);
    expect(groups[0].models.map((m) => m.model)).toEqual(['x', 'z']);
  });

  it('starts a new setup on the first checked model', () => {
    const s = blankSetup([account('a', [info('y', false), info('x', true)])]);
    expect(s).toMatchObject({ id: '', provider_id: 'a', model: 'x', name: 'X' });
  });

  it('round-trips a model key whose id has slashes and colons', () => {
    const key = modelKey('prov_1', 'vendor/model:free');
    expect(splitModelKey(key)).toEqual({ providerId: 'prov_1', model: 'vendor/model:free' });
  });

  it('finds the setups running a model', () => {
    const setups = [setup('1', 'a', 'x'), setup('2', 'a', 'y'), setup('3', 'b', 'x')];
    expect(setupsUsing(setups, 'a', 'x')).toEqual(['S1']);
    expect(setupsUsing(setups, 'a', 'z')).toEqual([]);
  });

  it('duplicates as a new setup', () => {
    expect(duplicateOf(setup('1', 'a', 'x'))).toMatchObject({ id: '', name: 'S1 (copy)' });
  });

  it('moves within bounds', () => {
    expect(moved([1, 2, 3], 0, 1)).toEqual([2, 1, 3]);
    expect(droppedAt([1, 2, 3], 0, 3)).toEqual([2, 3, 1]);
    expect(droppedAt([1, 2, 3], 2, 0)).toEqual([3, 1, 2]);
    expect(droppedAt([1, 2, 3], 1, 3)).toEqual([1, 3, 2]);
    const fixed = [1, 2, 3];
    expect(droppedAt(fixed, 1, 1)).toBe(fixed);
    expect(droppedAt(fixed, 1, 2)).toBe(fixed);
    expect(moved([1, 2, 3], 2, 1)).toEqual([1, 2, 3]);
    const same = [1, 2];
    expect(moved(same, 0, -1)).toBe(same);
  });
});
