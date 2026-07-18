// Message-assembly helpers: closing/opening turns, artifact derivation from
// tool calls, and matching a resumed session's stored provider/model back to
// a currently-configured Kitty provider profile.

import { tryParsePyRepr } from '@/lib/pyrepr';
import type { ProviderType, ProviderView, ToolCallUpdate } from '@/lib/types';
import type { AdaptivePathwayHint, Artifact, Message, ParsedHintOutput, ToolCall } from './types';

export const closeOpen = (msgs: Message[]): Message[] =>
  msgs.map((m) => (m.open ? { ...m, open: false, streaming: false } : m));

/** True when `last` is an already-closed assistant message from the turn that
    *just* finished, with nothing new started yet. A tool-call notification
    and the turn's completion response travel through separate tasks
    (goosed's reader loop resolves the completion oneshot on a different task
    than the one that emits `chat://tool-call`/reasoning/message deltas), so
    it's possible for `chat://complete` to reach the frontend and close the
    message a moment before a straggling event for that same turn arrives.
    Without this, the straggler spawned a brand-new, text-less assistant
    message instead of joining the box already shown above the real answer.
    Safe to fold back in: a genuinely new turn always pushes a user message
    first (via `send()`), so `last.role` would be `'user'` by then, not
    `'assistant'` — this can only match a true straggler. */
export function isStragglerAssistantMessage(last: Message | undefined, turnBusy: boolean): boolean {
  return !!last && last.role === 'assistant' && !last.open && !turnBusy;
}

/** Shared by `send()`'s dropped-file image split and `inlineFileAsAttachment`
    (chat-only mode's attach path) — one definition so the two can't drift. */
export function isImageFileName(name: string): boolean {
  return /\.(png|jpe?g|gif|webp|bmp)$/i.test(name);
}

/** `_meta.goose.toolCall.{toolName,extensionName}` (docs/acp-protocol.md) —
    shared by `deriveArtifact` and the `onToolCall` handler's `ToolCall`
    assembly, since both need to know which tool/extension a call belongs to. */
export function extractGooseToolMeta(u: ToolCallUpdate): {
  toolName?: string;
  extensionName?: string;
} {
  const meta = u as Record<string, unknown>;
  const goose = (
    meta._meta as { goose?: { toolCall?: { toolName?: string; extensionName?: string } } }
  )?.goose;
  return {
    toolName: goose?.toolCall?.toolName,
    extensionName: goose?.toolCall?.extensionName,
  };
}

// A file-writing tool by name/verb. Broadened (Round-5) beyond the original
// text_editor set to cover the write verbs other tools use.
const ARTIFACT_RE =
  /text_editor|write|create|edit|str_replace|insert|append|save|export|output|generate/i;
// A recognized artifact file by extension — the second, independent signal
// (Round-5): a tool that exposes an output path ending in one of these counts
// as an artifact even when its name doesn't match a write verb (e.g. a
// document/spreadsheet-producing tool). Covers the formats owners asked for
// (csv/xlsx/docx/md/json/py) plus common neighbors.
const ARTIFACT_EXT_RE =
  /\.(csv|tsv|xlsx?|xlsm|docx?|pptx?|md|markdown|json|jsonl|ya?ml|py|txt|html?|xml|pdf|rtf|odt|ods|odp|ipynb|sql|toml)$/i;
// Explicit read/inspect operations that also carry a `path` (e.g. text_editor
// `command:"view"`) — excluded so opening/reading a file never fabricates an
// artifact. This also fixes a latent false positive: `text_editor` matches
// ARTIFACT_RE by name, so a plain view used to register as a (bogus) artifact.
const READ_COMMAND_RE = /^(view|read|list|open|cat|show|inspect|search|find|glob|grep)$/i;

/** Make a tool-reported output path absolute so the Artifacts pane's Open /
    Copy path / Show-in-folder work (Round-5): goose reports a *relative* path
    for a write (e.g. `report.docx`), resolved against the session cwd — the
    chat folder. Without this the artifact stored just the bare filename and
    Open failed with "file not found". Absolute inputs (drive letter, unix
    root, UNC) are kept as-is. */
function absoluteArtifactPath(p: string, cwd: string | null): string {
  const isAbsolute = /^[a-z]:[\\/]/i.test(p) || p.startsWith('/') || p.startsWith('\\\\');
  if (isAbsolute || !cwd) return p;
  return `${cwd.replace(/[\\/]+$/, '')}/${p.replace(/^[\\/]+/, '')}`;
}

export function deriveArtifact(u: ToolCallUpdate, cwd: string | null = null): Artifact | null {
  const toolName = extractGooseToolMeta(u).toolName ?? '';
  const label = `${u.title ?? ''} ${toolName}`;
  const input = u.rawInput as
    { path?: string; file_path?: string; paths?: string[]; command?: string } | undefined;
  const p =
    input?.path ?? input?.file_path ?? (Array.isArray(input?.paths) ? input?.paths[0] : undefined);
  if (typeof p !== 'string' || !p) return null;
  // Never derive an artifact from an explicit read/view of a file.
  if (typeof input?.command === 'string' && READ_COMMAND_RE.test(input.command)) return null;
  // Qualify on either signal: a write-like tool name/verb, or a known artifact
  // file extension on the output path.
  if (!ARTIFACT_RE.test(label) && !ARTIFACT_EXT_RE.test(p)) return null;
  return {
    path: absoluteArtifactPath(p, cwd),
    name: p.split(/[\\/]/).pop() || p,
    tool: toolName || String(u.title || 'tool'),
    source: 'tool',
  };
}

/** Registers a user-attached file as an artifact (distinct from
    `deriveArtifact`'s tool-call-derived ones) — same dedup-by-path rule as the
    `onToolCall` handler, so a file that's later also written to by a tool
    doesn't produce a duplicate entry. */
export function userFileArtifact(path: string, name: string): Artifact {
  return { path, name, tool: 'attached', source: 'user' };
}

/** Mirrors `goose_provider_name` in `src-tauri/src/config/providers.rs` —
    goosed only ever sees `custom_openai` profiles as a plain `openai`
    provider (same client, just a different base URL), so that's the value
    it reports back in a session's stored `providerId`. */
function gooseProviderName(providerType: ProviderType): string {
  return providerType === 'custom_openai' ? 'openai' : providerType;
}

/** Finds the Kitty provider profile that produced a session's stored
    goosed-level `providerId`/`modelId` (`session/list`'s `_meta`, see
    `SessionSummary`) — used to restore the right provider when reopening an
    old chat. First match wins if the user has multiple profiles of the same
    type with the same model selected; goosed's own metadata has no way to
    disambiguate further than provider type + model id. `null` if nothing
    currently configured matches (e.g. the profile was deleted). */
export function findMatchingProvider(
  providers: ProviderView[],
  providerId: string,
  modelId: string
): ProviderView | null {
  return (
    providers.find(
      (p) => gooseProviderName(p.provider_type) === providerId && p.models.includes(modelId)
    ) ?? null
  );
}

/** Finds the Adaptive Pathway extension's `decide` tool call on a message, if
    any — hints correlate to messages "for free" this way, since `decide` is
    just another tool call on the same streaming assistant message (no new
    cross-message correlation scheme needed). Gated on `toolName` alone for
    now; tighten to also require a specific `extensionName` once confirmed
    live what Goose actually reports for this extension. */
export function findHintToolCall(msg: Message): ToolCall | undefined {
  return msg.toolCalls.find((t) => t.toolName === 'decide');
}

/** Parses a `decide` tool call's `output` (Python-repr text, see `pyrepr.ts`)
    into its hints. Returns `null` on anything unexpected — a malformed or
    still-streaming tool output should never crash message rendering. */
export function parseHintOutput(call: ToolCall | undefined): ParsedHintOutput | null {
  if (!call?.output) return null;
  const parsed = tryParsePyRepr(String(call.output)) as Record<string, unknown> | null;
  if (!parsed || !Array.isArray(parsed.hints)) return null;
  const hints = parsed.hints.filter(
    (h): h is AdaptivePathwayHint =>
      typeof h === 'object' && h !== null && typeof (h as { text?: unknown }).text === 'string'
  );
  if (hints.length === 0) return null;
  return {
    hints,
    confidence: typeof parsed.confidence === 'number' ? parsed.confidence : 0,
    novelty: typeof parsed.novelty === 'number' ? parsed.novelty : 0,
    nudge_offered: parsed.nudge_offered === true,
  };
}
