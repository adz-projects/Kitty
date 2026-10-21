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
    id: 'learn_gate',
    label: 'Memory: learn gate',
    blurb:
      'Skips Adaptive Pathway’s learning pass when a stretch of conversation says nothing lasting about you.',
  },
  {
    id: 'recall_rerank',
    label: 'Memory: recall relevance',
    blurb:
      'Checks which remembered beliefs actually bear on the message before they are added to the prompt.',
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
    id: 'search_rerank',
    label: 'Web: search re-rank',
    blurb: 'Orders merged web search results by how well they match the query.',
  },
  {
    id: 'shell_screen',
    label: 'Shell screen',
    blurb:
      'A second opinion on shell commands Kitty would otherwise run without asking: one that could delete files, change settings, install software, send data out or expose credentials is held for your approval. It can only add a prompt, never remove one.',
  },
  {
    id: 'compaction',
    label: 'Compaction',
    blurb:
      'Marks the exchanges that matter when a long chat is summarised, and leaves out pure filler.',
  },
];

/** Thresholds the user can override, with what each one means. */
export const DECIDER_THRESHOLDS: { key: string; label: string }[] = [
  { key: 'tool_gating_cold', label: 'Tool gating: load a group on a fresh decision at P ≥' },
  { key: 'tool_gating_add', label: 'Tool gating: add a group mid-chat at P ≥' },
  { key: 'learn_gate_skip_below', label: 'Learn gate: skip a learning pass below P' },
  { key: 'recall_drop_below', label: 'Recall: drop a belief below P' },
  { key: 'ingest_gate_reject_below', label: 'Ingest gate: reject a document below P' },
  { key: 'page_classify_min', label: 'Page check: act on a label at P ≥' },
  { key: 'compaction_drop_min', label: 'Compaction: leave out filler at P ≥' },
  { key: 'shell_screen_flag_at', label: 'Shell screen: ask before running at P(risky) ≥' },
];

/** The two web checks run inside kitty-web, whose counters the engine
    doesn't see. */
const COUNTED_IN_PLUGIN: DeciderFeature[] = ['page_classify', 'search_rerank'];

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
