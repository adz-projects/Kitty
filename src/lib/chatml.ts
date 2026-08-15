// Session export (Round-3 item 24). Produces a single-line `.jsonl` file with
// one JSON object: `{ messages: [{role, content}, ...] }` (OpenAI SFT format —
// Unsloth-style loaders iterate line-by-line regardless of line count).
// Reasoning stays inline in the assistant turn as `<think>...</think>`
// (omitted when absent); tool-call references are stripped entirely (SFT-clean
// — no tool-call data anywhere in the export, unlike the old sidecar).

import type { Message } from '@/stores/chatStore';

export interface ChatMessage {
  role: 'user' | 'assistant';
  content: string;
}

export function buildExport(messages: Message[], upToIndex?: number): ChatMessage[] {
  const slice = upToIndex != null ? messages.slice(0, upToIndex + 1) : messages;
  // Skip superseded turns (a regenerated-away-from answer, kept collapsed in
  // the UI) — exporting both the rejected answer and its replacement produces
  // a corrupted transcript with two assistant turns for one user turn.
  return slice
    .filter((m) => !m.superseded)
    .map((m) => {
      if (m.role === 'user') {
        return { role: 'user', content: m.text };
      }
      const think = m.reasoning ? `<think>\n${m.reasoning}\n</think>\n` : '';
      return { role: 'assistant', content: think + m.text };
    });
}

export function sanitizeFilename(name: string): string {
  return (name || 'kitty-session')
    .replace(/[\\/:*?"<>|]/g, '_')
    .trim()
    .slice(0, 80);
}
