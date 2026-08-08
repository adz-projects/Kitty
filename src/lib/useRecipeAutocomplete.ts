import { useEffect, useState } from 'react';
import type { Recipe } from './types';

/** Drives the composer's slash-command dropdown: "composing a slug" means the
    text starts with `/` and has no whitespace yet (once a space follows the
    slug, this naturally closes — the user has moved on to typing their
    request). Escape suppresses the dropdown for the *current* query only —
    typing further (changing the query) reopens it, matching typical
    command-palette behavior without needing a separate close/reopen dance
    once the query itself changes. */
export function useRecipeAutocomplete(text: string, recipes: Recipe[]) {
  const isComposingSlug = text.startsWith('/') && text.length > 1 && !/\s/.test(text);
  const query = isComposingSlug ? text.slice(1).toLowerCase() : '';
  const matches = isComposingSlug
    ? recipes.filter((r) => r.slug.toLowerCase().startsWith(query))
    : [];

  const [selectedIndex, setSelectedIndex] = useState(0);
  const [dismissedQuery, setDismissedQuery] = useState<string | null>(null);

  useEffect(() => {
    setSelectedIndex(0);
  }, [query]);

  // Clamp when the live recipe list shrinks under an active query (recipes
  // refresh across windows via `recipes://changed`) — otherwise a stale,
  // out-of-range index makes the composer's Enter/Tab accept silently miss
  // (`matches[selectedIndex]` is undefined) and submit the raw `/slug`.
  useEffect(() => {
    if (!matches.length) setSelectedIndex(0);
    else setSelectedIndex((i) => Math.min(i, matches.length - 1));
  }, [matches.length]);

  // A dismissal is only for the *current* spell of the query — once the user
  // clears back to `/` (query becomes empty), the next identical re-type is a
  // fresh attempt and must be able to open again (previously it stayed
  // suppressed forever, since the re-typed query equals the dismissed one).
  useEffect(() => {
    if (query === '') setDismissedQuery(null);
  }, [query]);

  const open = isComposingSlug && matches.length > 0 && query !== dismissedQuery;
  const dismiss = () => setDismissedQuery(query);

  return { open, matches, selectedIndex, setSelectedIndex, dismiss };
}
