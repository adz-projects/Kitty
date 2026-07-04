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
