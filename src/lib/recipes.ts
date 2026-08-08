// Pure helpers for Goose recipes — client-side-interpreted templates (see
// docs/BACKLOG.md's resolved recipes entry and chatStore.ts's sendWithRecipe).
// No chrome.*/tauri calls here; kept pure so it's trivially unit-testable and
// reusable from both Composer.tsx (autocomplete + submit parsing) and
// chatStore.ts (resolution + launch).

import type { Recipe, RecipeExtension, RecipeParameter } from './types';

/** Replaces every `{{ key }}` occurrence with `values[key]` (or '' if
    missing) — a hand-rolled substitution matching the real Goose recipe
    schema's Jinja-style templating, without pulling in a template engine for
    one trivial replace. The key class is widened past `\w` to `[\w.-]` so
    hyphenated/dotted parameter keys in the wild (`{{ user-input }}`,
    `{{ ref.pdf }}`) actually resolve instead of leaking the literal `{{ ... }}`
    into the model-visible prompt. */
export function substituteTemplate(text: string, values: Record<string, string>): string {
  return text.replace(/\{\{\s*([\w.-]+)\s*\}\}/g, (_match, key: string) => values[key] ?? '');
}

/** The one parameter (if any) whose value comes from whatever the user typed
    after `/slug` — real Goose's `requirement: "user_prompt"` already means
    "collect this interactively," which is exactly what the slash command's
    trailing free text is. Recipes are validated (`recipe_yaml::validate_recipe`
    on the Rust side) to have at most one; if more than one somehow slipped
    through, picking the first is a safe, deterministic fallback rather than
    throwing. */
export function primaryParameter(recipe: Recipe): RecipeParameter | undefined {
  return recipe.parameters.find((p) => p.requirement === 'user_prompt');
}

/** Parameters Kitty can't resolve at invocation time — not the primary one,
    and no default to fall back on. Kitty doesn't collect any parameter
    interactively except the primary one (a deliberate v1 scope cut, no
    multi-field dialog), so these always resolve empty. Surfaced as a "needs
    attention" badge in the slash-autocomplete dropdown and the Recipes
    settings panel; mirrors the same rule as the Rust-side warning in
    `recipe_yaml::validate_recipe`. */
export function recipeNeedsAttention(recipe: Recipe): string[] {
  const primary = primaryParameter(recipe);
  return recipe.parameters
    .filter((p) => p !== primary && !(p.default ?? '').trim())
    .map((p) => p.key);
}

/** Parses a leading `/slug` (optionally followed by free text) out of
    composer input. Returns `null` when the text doesn't start with `/` or no
    recipe matches that slug. Slug matching is case-insensitive; the trailing
    text is returned trimmed of its own leading whitespace only (interior
    whitespace/newlines are preserved verbatim, since that's the user's actual
    request). */
export function matchRecipeCommand(
  text: string,
  recipes: Recipe[]
): { recipe: Recipe; primaryText: string } | null {
  if (!text.startsWith('/')) return null;
  const withoutSlash = text.slice(1);
  const spaceIdx = withoutSlash.search(/\s/);
  const slugToken = spaceIdx === -1 ? withoutSlash : withoutSlash.slice(0, spaceIdx);
  if (!slugToken) return null;
  const recipe = recipes.find((r) => r.slug.toLowerCase() === slugToken.toLowerCase());
  if (!recipe) return null;
  const primaryText = spaceIdx === -1 ? '' : withoutSlash.slice(spaceIdx + 1).trimStart();
  return { recipe, primaryText };
}

/** Substitutes every declared parameter (primary parameter <- `primaryText`,
    everything else <- its own `default`) into `instructions`/`prompt`. */
export function resolveRecipe(
  recipe: Recipe,
  primaryText: string
): { resolvedInstructions: string | null; resolvedPromptText: string } {
  const primary = primaryParameter(recipe);
  const values: Record<string, string> = {};
  for (const p of recipe.parameters) {
    values[p.key] = p === primary ? primaryText.trim() || p.default || '' : (p.default ?? '');
  }
  const resolvedInstructions = recipe.instructions
    ? substituteTemplate(recipe.instructions, values)
    : null;
  const resolvedPromptText = recipe.prompt
    ? substituteTemplate(recipe.prompt, values)
    : primaryText.trim();
  return { resolvedInstructions, resolvedPromptText };
}

/** Extensions ACP can actually add to a live session — filters out the 3
    real-schema types with no ACP equivalent (`streamable_http`/`frontend`/
    `inline_python`) before even round-tripping to Rust for one. */
export function launchableExtensions(extensions: RecipeExtension[]): RecipeExtension[] {
  return extensions.filter(
    (e) => e.type === 'builtin' || e.type === 'platform' || e.type === 'stdio'
  );
}
