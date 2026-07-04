// Shared TS types. Mirror the Rust structs by hand; keep in sync.
// Pairs:
//   Config / NotificationPrefs  <-> src-tauri/src/config/mod.rs
//   StackStatus / StackStatusPayload <-> src-tauri/src/lifecycle/mod.rs

export interface NotificationPrefs {
  task_complete: boolean;
  approval_needed: boolean;
  task_failed: boolean;
  stack_degraded: boolean;
}

export interface Config {
  hotkey: string;
  use_copilot_key: boolean;
  default_context_folder: string | null;
  ollama_base_url: string;
  setup_completed: boolean;
  theme: string;
  notifications: NotificationPrefs;
  remember_overlay_position: boolean;
}

// Serde `rename_all = "snake_case"` on the Rust enum.
export type StackStatus =
  | 'starting'
  | 'ok'
  | 'ollama_down'
  | 'goosed_down'
  | 'no_model'
  | 'provider_unreachable'
  | 'conflict_goose_desktop';

export interface StackStatusPayload {
  status: StackStatus;
  detail: string | null;
}

// --- Chat / ACP (Phase 2) --- mirrors src-tauri/src/commands SessionInfo + events

export interface ModeInfo {
  id: string;
  name: string;
  description: string;
}

export interface SessionInfo {
  session_id: string;
  cwd: string;
  current_mode: string;
  available_modes: ModeInfo[];
}

/** Raw ACP tool-call `update` object (shape varies; read defensively).
    Confirmed live: `tool_call` carries title + rawInput; `tool_call_update`
    carries status + content + rawOutput; later updates may omit title/status. */
export interface ToolCallUpdate {
  toolCallId?: string;
  title?: string;
  kind?: string;
  status?: string;
  rawInput?: unknown;
  rawOutput?: unknown;
  content?: unknown;
  [key: string]: unknown;
}

export interface TextDeltaEvent {
  session_id: string;
  text: string;
}

export interface ToolCallEvent {
  session_id: string;
  phase: 'tool_call' | 'tool_call_update';
  update: ToolCallUpdate;
}

export interface SessionTitleEvent {
  session_id: string;
  title: string;
}

export interface CompleteEvent {
  session_id: string;
  result: { stopReason?: string; usage?: Record<string, number> };
}

export interface ChatErrorEvent {
  session_id: string;
  message: string;
}

// --- Approvals / modes (Phase 3) ---

export interface ApprovalOption {
  optionId: string;
  name: string;
  kind: string;
}

export interface ApprovalToolCall {
  toolCallId?: string;
  title?: string;
  kind?: string;
  status?: string;
  rawInput?: unknown;
  [key: string]: unknown;
}

export interface ApprovalNeededEvent {
  session_id: string;
  tool_call_id: string;
  tool_call: ApprovalToolCall;
  options: ApprovalOption[];
}

export interface ModeEvent {
  session_id: string;
  mode: string;
}

// --- Sessions / filesystem (Phase 4) ---

/** Mirrors src-tauri PathInfo. */
export interface PathInfo {
  path: string;
  name: string;
  is_dir: boolean;
  exists: boolean;
}

/** Parsed from a raw ACP `session/list` entry. */
export interface SessionSummary {
  sessionId: string;
  title: string;
  cwd: string;
  updatedAt: string;
  messageCount?: number;
  providerId?: string;
  modelId?: string;
}

/** Parse a raw session/list object (see docs/acp-protocol.md) defensively. */
export function parseSession(raw: Record<string, unknown>): SessionSummary {
  const meta = (raw._meta as Record<string, unknown>) ?? {};
  return {
    sessionId: String(raw.sessionId ?? ''),
    title: String(raw.title ?? 'Untitled session'),
    cwd: String(raw.cwd ?? ''),
    updatedAt: String(raw.updatedAt ?? ''),
    messageCount: typeof meta.messageCount === 'number' ? meta.messageCount : undefined,
    providerId: typeof meta.providerId === 'string' ? meta.providerId : undefined,
    modelId: typeof meta.modelId === 'string' ? meta.modelId : undefined,
  };
}
