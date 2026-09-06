import { describe, it, expect } from 'vitest';
import { isConnectivityError, isProviderScopedError } from './chatStore';

/** Backs the fix for an error card that outlived the problem it described:
    the `providerOffline` banner cleared when the provider came back, but the
    "Can't reach provider" card underneath it stayed until a restart. These
    two predicates are what decides which cards a recovery retires. */

describe('isConnectivityError', () => {
  it('matches the classified unreachable-provider error', () => {
    expect(isConnectivityError('dial tcp: no route to host', 'network_unreachable')).toBe(true);
  });

  it('matches an unclassified lost-connection error', () => {
    expect(isConnectivityError('ACP connection closed')).toBe(true);
  });

  it('does not match a timeout', () => {
    // A reachable provider says nothing about whether the request that timed
    // out would now succeed, so that card has to stay.
    expect(isConnectivityError('ACP request timed out (no response for 5 minutes)')).toBe(false);
  });

  it('does not match another classified error that merely mentions connecting', () => {
    expect(isConnectivityError('could not connect to billing', 'insufficient_credits')).toBe(false);
  });

  it('is false when there is no error at all', () => {
    expect(isConnectivityError(null)).toBe(false);
    expect(isConnectivityError(null, 'network_unreachable')).toBe(false);
  });
});

describe('isProviderScopedError', () => {
  it('covers the errors that belong to a provider, not a conversation', () => {
    expect(isProviderScopedError('network_unreachable')).toBe(true);
    expect(isProviderScopedError('auth_failed')).toBe(true);
    expect(isProviderScopedError('insufficient_credits')).toBe(true);
  });

  it('leaves a conversation-level failure alone', () => {
    // Switching providers doesn't shorten the conversation.
    expect(isProviderScopedError('context_exceeded')).toBe(false);
    expect(isProviderScopedError(null)).toBe(false);
    expect(isProviderScopedError(undefined)).toBe(false);
  });
});
