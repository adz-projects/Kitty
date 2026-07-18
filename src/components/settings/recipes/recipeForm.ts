// Pure form-state helpers for the Recipes editor: slug derivation and the
// FormState <-> Recipe/RecipeInput conversions.

import type { ParameterInputType, Recipe, RecipeExtension, RecipeInput, RecipeParameter } from '@/lib/types';

export const NON_PRIMARY_INPUT_TYPES: ParameterInputType[] = [
  'string',
  'number',
  'boolean',
  'date',
  'select',
];

/** Matches `config::recipes::default_max_reasoning_tokens()` — used when
    seeding a blank/template-derived draft. */
export const DEFAULT_MAX_REASONING_TOKENS = 2048;

export function slugify(title: string): string {
  const base = title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '_')
    .replace(/^_+|_+$/g, '')
    .replace(/^[0-9_]+/, '');
  return base || 'recipe';
}

export interface ActivityRow {
  text: string;
  isMessage: boolean;
}

export interface FormState {
  slug: string;
  slugTouched: boolean;
  title: string;
  description: string;
  instructions: string;
  prompt: string;
  primaryKey: string;
  primaryDescription: string;
  parameters: RecipeParameter[]; // non-primary only
  extensions: RecipeExtension[];
  activities: ActivityRow[];
  maxReasoningTokens: number;
}

export function blankForm(): FormState {
  return {
    slug: '',
    slugTouched: false,
    title: '',
    description: '',
    instructions: '',
    prompt: '',
    primaryKey: 'request',
    primaryDescription: '',
    parameters: [],
    extensions: [],
    activities: [],
    maxReasoningTokens: DEFAULT_MAX_REASONING_TOKENS,
  };
}

export function formFromRecipe(r: Recipe): FormState {
  const primary = r.parameters.find((p) => p.requirement === 'user_prompt');
  return {
    slug: r.slug,
    slugTouched: true,
    title: r.title,
    description: r.description,
    instructions: r.instructions ?? '',
    prompt: r.prompt ?? '',
    primaryKey: primary?.key ?? 'request',
    primaryDescription: primary?.description ?? '',
    parameters: r.parameters.filter((p) => p !== primary),
    extensions: r.extensions,
    activities: r.activities.map((a) =>
      a.startsWith('message:')
        ? { text: a.slice(8).trim(), isMessage: true }
        : { text: a, isMessage: false }
    ),
    maxReasoningTokens: r.max_reasoning_tokens,
  };
}

export function formToInput(form: FormState): RecipeInput {
  const primaryParam: RecipeParameter = {
    key: form.primaryKey.trim() || 'request',
    input_type: 'string',
    requirement: 'user_prompt',
    description: form.primaryDescription.trim(),
    default: null,
    options: [],
  };
  return {
    slug: form.slug,
    title: form.title.trim(),
    description: form.description.trim(),
    instructions: form.instructions.trim() || null,
    prompt: form.prompt.trim() || null,
    parameters: [primaryParam, ...form.parameters],
    extensions: form.extensions,
    max_reasoning_tokens: Math.max(1, Math.round(form.maxReasoningTokens) || DEFAULT_MAX_REASONING_TOKENS),
    activities: form.activities
      .filter((a) => a.text.trim())
      .map((a) => (a.isMessage ? `message: ${a.text.trim()}` : a.text.trim())),
  };
}

export function blankParameter(): RecipeParameter {
  return {
    key: '',
    input_type: 'string',
    requirement: 'optional',
    description: '',
    default: '',
    options: [],
  };
}
