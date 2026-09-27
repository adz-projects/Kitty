import { describe, expect, it } from 'vitest';
import { needsHandoffGate, rememberedChoice, trustRank } from './handoff';

const local = { network_tier: 'local', is_trusted: true } as const;
const trustedRemote = { network_tier: 'remote', is_trusted: true } as const;
const tailnet = { network_tier: 'personal', is_trusted: false } as const;
const remote = { network_tier: 'remote', is_trusted: false } as const;

describe('handoff gate', () => {
  it('ranks cards by how far the conversation would travel', () => {
    expect([local, trustedRemote, tailnet, remote].map(trustRank)).toEqual([0, 1, 2, 3]);
  });

  it('asks only when the target is less trusted', () => {
    expect(needsHandoffGate(local, remote)).toBe(true);
    expect(needsHandoffGate(remote, local)).toBe(false);
    expect(needsHandoffGate(trustedRemote, trustedRemote)).toBe(false);
    expect(needsHandoffGate(tailnet, remote)).toBe(true);
    expect(needsHandoffGate(undefined, trustedRemote)).toBe(true);
  });

  it('only accepts a real remembered answer', () => {
    expect(rememberedChoice('keep')).toBe('keep');
    expect(rememberedChoice('clean')).toBe('clean');
    expect(rememberedChoice(null)).toBeNull();
    expect(rememberedChoice('bogus')).toBeNull();
  });
});
