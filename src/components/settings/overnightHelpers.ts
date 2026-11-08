import type { OvernightReport } from '@/lib/types';

/** One sentence about what a pass did. */
export function describeReport(r: OvernightReport): string {
  const parts: string[] = [];
  if (r.candidates_added > 0)
    parts.push(`${r.candidates_added} new passage${r.candidates_added === 1 ? '' : 's'} set aside`);
  if (r.clusters_promoted > 0)
    parts.push(
      `${r.clusters_promoted} recurring topic${r.clusters_promoted === 1 ? '' : 's'} learned`
    );
  if (r.propositions_written > 0)
    parts.push(`${r.propositions_written} fact${r.propositions_written === 1 ? '' : 's'} written`);
  if (r.candidates_expired > 0)
    parts.push(
      `${r.candidates_expired} passage${r.candidates_expired === 1 ? '' : 's'} expired unused`
    );
  const what = parts.length > 0 ? parts.join(', ') : 'nothing new';
  return r.finished ? `${what}.` : `${what} (stopped before finishing; it resumes next time).`;
}

/** "today at 01:12", "yesterday at 02:40", or a date, in local time. */
export function describeWhen(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const day = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diff = Math.round((day(now) - day(d)) / 86_400_000);
  if (diff === 0) return `today at ${time}`;
  if (diff === 1) return `yesterday at ${time}`;
  return `${d.toLocaleDateString()} at ${time}`;
}

/** Why Windows won't wake the computer for the task, or null. */
export function wakeTimerWarning(t: { ac: boolean | null; dc: boolean | null }): string | null {
  if (t.ac === false)
    return 'Wake timers are turned off in this power plan, so Windows won’t wake the computer for this. Turn on Power Options → Change advanced power settings → Sleep → Allow wake timers (Enable).';
  return null;
}
