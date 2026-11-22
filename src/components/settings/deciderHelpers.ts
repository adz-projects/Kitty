import type { DeciderFeature, DeciderFeatureStats, DeciderSettings } from '@/lib/types';

/** Every feature, in the order Settings lists them, with what it does. */
export const DECIDER_FEATURES: { id: DeciderFeature; label: string; blurb: string }[] = [
  {
    id: 'tool_gating',
    label: 'Tool gating',
    blurb: 'Offers each chat only the tool groups it needs, without breaking the prompt cache.',
  },
  {
    id: 'ingest_gate',
    label: 'Memory: paragraph screen',
    blurb: 'Lets only lasting, well-supported paragraphs from a chat into Memorabilia.',
  },
  {
    id: 'page_classify',
    label: 'Web: page check',
    blurb: 'Spots bot challenges, paywalls and error pages in scraped pages.',
  },
  {
    id: 'shell_screen',
    label: 'Shell screen',
    blurb:
      'Holds a risky shell command for your approval that Kitty would otherwise run; it only ever adds a prompt.',
  },
];

/** Thresholds the user can override, with what each one means. */
export const DECIDER_THRESHOLDS: { key: string; label: string }[] = [
  { key: 'tool_gating_cold', label: 'Tool gating: load a group on a fresh decision at P ≥' },
  { key: 'tool_gating_add', label: 'Tool gating: add a group mid-chat at P ≥' },
  { key: 'page_classify_min', label: 'Page check: act on a label at P ≥' },
  { key: 'shell_screen_flag_at', label: 'Shell screen: ask before running at P(risky) ≥' },
];

/** The page check runs inside kitty-web, whose counters the engine doesn't
    see. */
const COUNTED_IN_PLUGIN: DeciderFeature[] = ['page_classify'];

/** One line of counters for a feature, or null when there is nothing to say. */
export function statLine(
  feature: DeciderFeature,
  stats: DeciderFeatureStats | undefined
): string | null {
  if (COUNTED_IN_PLUGIN.includes(feature)) return null;
  if (!stats || stats.calls === 0) return null;
  const parts = [`${stats.calls} asked`, `${stats.acted} acted on`];
  if (stats.failed > 0) parts.push(`${stats.failed} fell back`);
  return `Since the engine started: ${parts.join(' · ')}`;
}

/** Whether a tested model differs from the one the settings were tuned for. */
export function modelChanged(saved: DeciderSettings, testedModel: string | null): boolean {
  return saved.model != null && testedModel != null && saved.model !== testedModel;
}

/** Parse a threshold field: '' clears the override, anything else must be a
    probability. Returns undefined for invalid input. */
export function parseThreshold(text: string): number | null | undefined {
  const t = text.trim();
  if (t === '') return null;
  const n = Number(t);
  if (!Number.isFinite(n) || n < 0 || n > 1) return undefined;
  return n;
}
