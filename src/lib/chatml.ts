// ChatML export (Phase 11). Produces a plain-text `.chatml` file plus a sidecar
// `.meta.json`. Reasoning goes in <think>…</think> inside the assistant turn
// (omitted when absent — no empty blocks); tool calls appear as a minimal inline
// note referencing the sidecar, which holds their full detail.

import type { Message } from '@/stores/chatStore';

export interface ExportMeta {
  sessionId: string | null;
  title: string | null;
  workingDir: string | null;
  model: string | null;
}

interface MetaToolCall {
  index: number;
  turn: number;
  name: string;
  params: unknown;
  result: unknown;
}

export function buildExport(messages: Message[], meta: ExportMeta, upToIndex?: number) {
  const slice = upToIndex != null ? messages.slice(0, upToIndex + 1) : messages;
  const toolCalls: MetaToolCall[] = [];
  const turns: { index: number; role: string; model: string | null }[] = [];
  const blocks: string[] = [];

  slice.forEach((m, i) => {
    turns.push({ index: i, role: m.role, model: meta.model });
    if (m.role === 'user') {
      blocks.push(`<|im_start|>user\n${m.text}<|im_end|>`);
      return;
    }
    let body = '';
    for (const tc of m.toolCalls) {
      const idx = toolCalls.length;
      toolCalls.push({ index: idx, turn: i, name: tc.title, params: tc.input, result: tc.output });
      body += `[tool_call: ${tc.title} → see meta.json#tool_calls[${idx}]]\n`;
    }
    if (m.reasoning) body += `<think>\n${m.reasoning}\n</think>\n`;
    body += m.text;
    blocks.push(`<|im_start|>assistant\n${body}<|im_end|>`);
  });

  const metaObj = {
    sessionId: meta.sessionId,
    title: meta.title,
    workingDir: meta.workingDir,
    model: meta.model,
    exportedAt: new Date().toISOString(),
    turns,
    toolCalls,
  };
  return { chatml: blocks.join('\n'), meta: metaObj };
}

export function sanitizeFilename(name: string): string {
  return (name || 'goose-session')
    .replace(/[\\/:*?"<>|]/g, '_')
    .trim()
    .slice(0, 80);
}
