import type { DeciderFeature, DeciderFeatureStats, DeciderSettings } from '@/lib/types';

/** Every feature, in the order Settings lists them, with what it does. */
export const DECIDER_FEATURES: { id: DeciderFeature; label: string; blurb: string }[] = [
  {
    id: 'tool_gating',
    label: 'Tool gating',
    blurb:
      'Offers each chat only the tool groups it needs (documents, web, charts, code…). Groups are added as a chat needs them and only pruned when the provider cache has gone cold anyway, so cache hits are kept.',
  },
  {
    id: 'ingest_gate',
    label: 'Memory: ingest gate',
    blurb: 'Keeps boilerplate, error pages and chit-chat out of Memorabilia’s factual memory.',
  },
  {
    id: 'page_classify',
    label: 'Web: page check',
    blurb:
      'Recognises bot challenges, paywalls and error pages when a page is scraped, instead of handing them to the model as content.',
  },
  {
    id: 'shell_screen',
    label: 'Shell screen',
    blurb:
      'A second opinion on shell commands Kitty would otherwise run without asking: one that could delete files, change settings, install software, send data out or expose credentials is held for your approval. It can only add a prompt, never remove one.',
  },
];

/** Thresholds the user can override, with what each one means. */
export const DECIDER_THRESHOLDS: { key: string; label: string }[] = [
  { key: 'tool_gating_cold', label: 'Tool gating: load a group on a fresh decision at P ≥' },
  { key: 'tool_gating_add', label: 'Tool gating: add a group mid-chat at P ≥' },
  { key: 'ingest_gate_reject_below', label: 'Ingest gate: reject a document below P' },
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
