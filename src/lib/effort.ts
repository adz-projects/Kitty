import type { EffortOption, EffortSummary, ThinkingEffort } from './types';

/** The option the picker shows as selected. Options can shrink after a card
    change, so a `current_value` that is no longer offered falls back to the
    first option for display (the store keeps the backend's value untouched). */
export function selectedEffort(effort: ThinkingEffort): EffortOption | null {
  return (
    effort.options.find((o) => o.value === effort.current_value) ?? effort.options[0] ?? null
  );
}

/** The accessible label for one option, e.g. "Reasoning: High (3 of 5)" or
    "Reasoning: Off". The stars are the visual; this is what a screen reader
    and the tooltip say. */
export function effortLabel(option: EffortOption, maxStars: number): string {
  if (option.stars === 0) return `Reasoning: ${option.name}`;
  if (maxStars <= 1) return `Reasoning: ${option.name}`;
  return `Reasoning: ${option.name} (${option.stars} of ${maxStars})`;
}

function ago(seconds: number): string {
  if (seconds < 60) return 'just now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} h ago`;
  const days = Math.floor(seconds / 86400);
  return days === 1 ? '1 day ago' : `${days} days ago`;
}

/** The Settings → Providers line for a card's detected reasoning ladder, e.g.
    "Reasoning: Off + 5 levels (low → max) · via OpenRouter · 12 days ago". */
export function effortSummaryText(s: EffortSummary, nowSecs: number): string {
  if (s.detected_at === null) {
    return s.detecting ? 'Detecting reasoning levels…' : 'Reasoning levels not detected yet';
  }
  let what: string;
  if (s.levels.length === 0 && !s.can_disable) {
    what = 'No reasoning control';
  } else if (s.levels.length === 0 || (s.levels.length === 1 && s.levels[0] === 'on')) {
    what = 'Reasoning: Off / On';
  } else {
    const n = s.levels.length;
    const range = n > 1 ? ` (${s.levels[0]} → ${s.levels[n - 1]})` : ` (${s.levels[0]})`;
    what = `Reasoning: ${s.can_disable ? 'Off + ' : ''}${n} level${n === 1 ? '' : 's'}${range}`;
  }
  const via = s.source_label ? ` · via ${s.source_label}` : '';
  return `${what}${via} · ${ago(Math.max(0, nowSecs - s.detected_at))}`;
}
