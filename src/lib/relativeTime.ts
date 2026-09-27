/** The engine's timestamps are SQLite's naive `YYYY-MM-DD HH:MM:SS`, in UTC. */
export function parseEngineTime(value: string): number | null {
  if (!value) return null;
  const iso = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}/.test(value) ? `${value.replace(' ', 'T')}Z` : value;
  const t = Date.parse(iso);
  return Number.isNaN(t) ? null : t;
}

/** "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago", else a
    date. Pure (`now` is passed in). */
export function relativeTime(value: string, now: number): string {
  const t = parseEngineTime(value);
  if (t === null) return '';
  const secs = Math.max(0, Math.round((now - t) / 1000));
  if (secs < 60) return 'just now';
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.floor(hours / 24);
  if (days === 1) return 'yesterday';
  if (days < 7) return `${days} days ago`;
  return new Date(t).toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    ...(days > 300 ? { year: 'numeric' } : {}),
  });
}
