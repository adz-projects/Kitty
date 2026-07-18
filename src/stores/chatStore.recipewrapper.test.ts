import { describe, it, expect } from 'vitest';
import { stripRecipeWrapper, stripPromptPreamble } from './chatStore';

/** Backs the fix for the hidden `<recipe>` wrapper leaking into the visible
    bubble on session replay. `sendWithRecipe` prepends a `<recipe>…</recipe>`
    + "Run the recipe above now…" preamble to the transmitted prompt; goosed
    stores exactly what was sent, so on resume it must be stripped back out.
    Unlike the system-prompt wrapper (first turn only), a recipe can be invoked
    on any turn, so this strips on every replayed user message. */

const RUN_LINE =
  'Run the recipe above now — it is mandatory for this message. You may use the ' +
  "conversation so far if it's relevant, but you are not required to.";

function recipeWrapped(title: string, instructions: string, body: string): string {
  return `<recipe title="${title}">\n${instructions}\n</recipe>\n\n${RUN_LINE}\n\n${body}`;
}

describe('stripRecipeWrapper', () => {
  it('strips a recipe wrapper, leaving the kick-off prompt', () => {
    const wrapped = recipeWrapped('Debate moderator', 'You are moderating a debate.', 'Motion: X');
    expect(stripRecipeWrapper(wrapped)).toBe('Motion: X');
  });

  it('handles multi-line instructions', () => {
    const wrapped = recipeWrapped(
      'Doc',
      'Line one.\nLine two.\nLine three.',
      'Create docs for: foo'
    );
    expect(stripRecipeWrapper(wrapped)).toBe('Create docs for: foo');
  });

  it('returns plain text unchanged when no recipe wrapper is present', () => {
    expect(stripRecipeWrapper('just a normal message')).toBe('just a normal message');
  });

  it('does not strip when text merely mentions <recipe> without the full shape', () => {
    const text = 'What does a <recipe> block look like?';
    expect(stripRecipeWrapper(text)).toBe(text);
  });

  it('composes with stripPromptPreamble on a recipe-invoked first turn (recipe wraps system)', () => {
    // First-turn order in send(): system wraps the user text, then the recipe
    // card wraps that — so the recipe wrapper is outermost.
    const inner = '<system>\nYou are a capable assistant.\n</system>\n\nMotion: X';
    const wrapped = recipeWrapped('Debate', 'You are moderating.', inner);
    // Strip recipe first, then the system preamble — recovers the real text.
    expect(stripPromptPreamble(stripRecipeWrapper(wrapped))).toBe('Motion: X');
  });

  it('leaves the system wrapper intact if only the recipe wrapper is stripped', () => {
    const inner = '<system>\nSys.\n</system>\n\nMotion: X';
    const wrapped = recipeWrapped('Debate', 'Instr.', inner);
    expect(stripRecipeWrapper(wrapped)).toBe(inner);
  });
});
