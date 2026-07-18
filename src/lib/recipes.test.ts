import { describe, it, expect } from 'vitest';
import {
  substituteTemplate,
  primaryParameter,
  recipeNeedsAttention,
  matchRecipeCommand,
  resolveRecipe,
  launchableExtensions,
} from './recipes';
import type { Recipe, RecipeParameter, RecipeExtension } from './types';

function param(overrides: Partial<RecipeParameter> = {}): RecipeParameter {
  return {
    key: 'topic',
    input_type: 'string',
    requirement: 'user_prompt',
    description: '',
    default: null,
    options: [],
    ...overrides,
  };
}

function recipe(overrides: Partial<Recipe> = {}): Recipe {
  return {
    id: 'recipe_1',
    slug: 'test_recipe',
    title: 'Test Recipe',
    description: 'A test recipe.',
    instructions: 'Do the thing about {{topic}} with style {{style}}.',
    prompt: 'Please cover: {{topic}}',
    version: '1.0.0',
    parameters: [param(), param({ key: 'style', requirement: 'optional', default: 'formal' })],
    extensions: [],
    activities: [],
    is_builtin: false,
    created_at: '2026-01-01T00:00:00Z',
    max_reasoning_tokens: 2048,
    ...overrides,
  };
}

describe('substituteTemplate', () => {
  it('replaces every occurrence of a declared variable', () => {
    expect(substituteTemplate('{{a}} and {{a}} and {{ b }}', { a: 'x', b: 'y' })).toBe(
      'x and x and y'
    );
  });

  it('replaces an unmatched variable with an empty string', () => {
    expect(substituteTemplate('Hello {{name}}!', {})).toBe('Hello !');
  });
});

describe('primaryParameter', () => {
  it('finds the user_prompt parameter', () => {
    const r = recipe();
    expect(primaryParameter(r)?.key).toBe('topic');
  });

  it('returns undefined when no parameter is user_prompt', () => {
    const r = recipe({ parameters: [param({ requirement: 'optional', default: 'x' })] });
    expect(primaryParameter(r)).toBeUndefined();
  });
});

describe('recipeNeedsAttention', () => {
  it('is empty when every non-primary parameter has a default', () => {
    expect(recipeNeedsAttention(recipe())).toEqual([]);
  });

  it('flags a non-primary parameter with no default', () => {
    const r = recipe({
      parameters: [param(), param({ key: 'missing', requirement: 'required', default: null })],
    });
    expect(recipeNeedsAttention(r)).toEqual(['missing']);
  });

  it('treats a blank-string default the same as no default', () => {
    const r = recipe({
      parameters: [param(), param({ key: 'blank', requirement: 'optional', default: '   ' })],
    });
    expect(recipeNeedsAttention(r)).toEqual(['blank']);
  });
});

describe('matchRecipeCommand', () => {
  const recipes = [recipe({ slug: 'annotated_bibliography' }), recipe({ slug: 'debate_moderator', id: 'r2' })];

  it('returns null when the text does not start with a slash', () => {
    expect(matchRecipeCommand('annotated_bibliography find sources', recipes)).toBeNull();
  });

  it('returns null when the slug matches no recipe', () => {
    expect(matchRecipeCommand('/unknown_recipe hello', recipes)).toBeNull();
  });

  it('matches a slug case-insensitively and extracts the trailing text', () => {
    const result = matchRecipeCommand('/Annotated_Bibliography find sources on AI', recipes);
    expect(result?.recipe.slug).toBe('annotated_bibliography');
    expect(result?.primaryText).toBe('find sources on AI');
  });

  it('returns an empty primaryText when the slug has no trailing text', () => {
    const result = matchRecipeCommand('/debate_moderator', recipes);
    expect(result?.recipe.slug).toBe('debate_moderator');
    expect(result?.primaryText).toBe('');
  });

  it('preserves interior whitespace/newlines in the trailing text', () => {
    const result = matchRecipeCommand('/debate_moderator line one\nline two', recipes);
    expect(result?.primaryText).toBe('line one\nline two');
  });
});

describe('resolveRecipe', () => {
  it('substitutes the primary parameter from typed text and others from defaults', () => {
    const { resolvedInstructions, resolvedPromptText } = resolveRecipe(
      recipe(),
      'AI in education'
    );
    expect(resolvedInstructions).toBe('Do the thing about AI in education with style formal.');
    expect(resolvedPromptText).toBe('Please cover: AI in education');
  });

  it('falls back to the primary default when no text was typed', () => {
    const r = recipe({ parameters: [param({ default: 'fallback topic' }), param({ key: 'style', requirement: 'optional', default: 'formal' })] });
    const { resolvedPromptText } = resolveRecipe(r, '');
    expect(resolvedPromptText).toBe('Please cover: fallback topic');
  });

  it('uses the typed text as the prompt when the recipe declares no prompt template', () => {
    const r = recipe({ prompt: null });
    const { resolvedPromptText } = resolveRecipe(r, 'my raw request');
    expect(resolvedPromptText).toBe('my raw request');
  });

  it('returns null resolvedInstructions when the recipe declares no instructions', () => {
    const r = recipe({ instructions: null });
    const { resolvedInstructions } = resolveRecipe(r, 'x');
    expect(resolvedInstructions).toBeNull();
  });
});

describe('launchableExtensions', () => {
  function ext(type: RecipeExtension['type']): RecipeExtension {
    return { type, name: 'x', args: [], env_keys: [] };
  }

  it('keeps builtin, platform, and stdio types', () => {
    const result = launchableExtensions([ext('builtin'), ext('platform'), ext('stdio')]);
    expect(result).toHaveLength(3);
  });

  it('filters out types with no ACP equivalent', () => {
    const result = launchableExtensions([
      ext('streamable_http'),
      ext('frontend'),
      ext('inline_python'),
      ext('builtin'),
    ]);
    expect(result).toEqual([ext('builtin')]);
  });
});
