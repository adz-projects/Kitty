import { describe, it, expect } from 'vitest';
import { humanizeChatError } from './chatStore';

/** Backs the fix for a raw ACP/JSON-RPC error (e.g. a bare "Invalid params")
    showing up in the chat error banner with no explanation of what happened
    or what to do about it. */

describe('humanizeChatError', () => {
  it('explains a raw "Invalid params" error', () => {
    expect(humanizeChatError('Invalid params')).toMatch(/switching providers|restarting the engine/);
  });

  it('explains a timeout error', () => {
    expect(humanizeChatError('ACP request timed out (no response for 5 minutes)')).toMatch(
      /took too long/
    );
  });

  it('explains a closed-connection error', () => {
    expect(humanizeChatError('ACP connection closed')).toMatch(/reconnect/i);
  });

  it('falls back to a generic message for an unrecognized error', () => {
    expect(humanizeChatError('some never-seen-before backend error')).toBe(
      'Something went wrong sending that message.'
    );
  });

  it('is case-insensitive', () => {
    expect(humanizeChatError('INVALID PARAMS')).toMatch(/switching providers|restarting the engine/);
  });

  it('uses the structured context_exceeded message when errorType is given', () => {
    expect(humanizeChatError('raw provider text', 'context_exceeded')).toMatch(
      /context limit/
    );
  });

  it('uses the structured insufficient_credits message when errorType is given', () => {
    expect(humanizeChatError('raw provider text', 'insufficient_credits')).toMatch(
      /credits are exhausted/
    );
  });

  it('falls through to string matching for an unknown errorType', () => {
    expect(humanizeChatError('ACP connection closed', 'other')).toMatch(/reconnect/i);
  });

  it('falls through to string matching with no errorType (backward compat)', () => {
    expect(humanizeChatError('Invalid params')).toMatch(/switching providers|restarting the engine/);
  });
});
