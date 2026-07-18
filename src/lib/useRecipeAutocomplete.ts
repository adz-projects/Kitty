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

  const open = isComposingSlug && matches.length > 0 && query !== dismissedQuery;
  const dismiss = () => setDismissedQuery(query);

  return { open, matches, selectedIndex, setSelectedIndex, dismiss };
}
