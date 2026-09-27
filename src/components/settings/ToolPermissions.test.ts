import { describe, expect, it } from 'vitest';
import { ruleScope } from './ToolPermissions';

describe('ruleScope', () => {
  it('names a tool-wide rule', () => {
    expect(ruleScope({ tool_name: 'lean_file_write', args_pattern: null })).toBe(
      'every use of lean_file_write'
    );
  });

  it('reads the command prefix back out of a shell rule', () => {
    // The pattern `approvals::always_scope` builds for `git status`.
    const pattern = String.raw`"command":"git status(?:"|\s|\\[nt])`;
    expect(ruleScope({ tool_name: 'shell', args_pattern: pattern })).toBe(
      'commands starting “git status”'
    );
    const escaped = String.raw`"command":"npm run\-build(?:"|\s|\\[nt])`;
    expect(ruleScope({ tool_name: 'shell', args_pattern: escaped })).toBe(
      'commands starting “npm run-build”'
    );
  });

  it('falls back to the raw pattern', () => {
    expect(ruleScope({ tool_name: 'x', args_pattern: 'odd' })).toBe('odd');
  });
});
