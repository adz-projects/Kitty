/** How long a memory engine may go without finishing any work, while work
    is waiting, before the Health panel says it has stalled. Background
    passes run every minute, so a day of nothing is a fault, not a queue. */
export const STALL_HOURS = 24;

const HOUR_MS = 3_600_000;

/** Parse an engine timestamp. The engine writes both `…Z` and
    `…T12:25:25.187486100+00:00` (nanoseconds), and the latter is not a
    format `Date.parse` is required to accept, so fractions are cut to
    milliseconds first. `null` for a missing or unreadable value. */
export function parseEngineTime(value: string | null | undefined): number | null {
  if (!value) {
    return null;
  }
  const trimmed = value.replace(/(\.\d{3})\d+/, '$1').replace(' ', 'T');
  const ms = Date.parse(trimmed);
  return Number.isNaN(ms) ? null : ms;
}

/** True when work is waiting and nothing has finished for `STALL_HOURS`
    (or ever). An empty backlog is never a stall, however old the last run. */
export function isStalled(
  backlog: number,
  lastAt: string | null | undefined,
  now: number = Date.now()
): boolean {
  if (backlog <= 0) {
    return false;
  }
  const last = parseEngineTime(lastAt);
  return last === null || now - last > STALL_HOURS * HOUR_MS;
}

/** "3 hours ago", "2 days ago", "never" — for the last-run line. */
export function describeAge(at: string | null | undefined, now: number = Date.now()): string {
  const last = parseEngineTime(at);
  if (last === null) {
    return 'never';
  }
  const hours = Math.max(0, Math.floor((now - last) / HOUR_MS));
  if (hours < 1) {
    return 'within the last hour';
  }
  if (hours < 48) {
    return `${hours} hour${hours === 1 ? '' : 's'} ago`;
  }
  return `${Math.floor(hours / 24)} days ago`;
}
