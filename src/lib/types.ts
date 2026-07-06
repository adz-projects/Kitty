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
  hotkeys: string[];
  use_copilot_key: boolean;
  default_context_folder: string | null;
  ollama_base_url: string;
  setup_completed: boolean;
  theme: string;
  background_image: string | null;
  background_dim: number;
  notifications: NotificationPrefs;
  remember_overlay_position: boolean;
  providers: ProviderProfile[];
  active_provider_id: string | null;
  strict_remote_mode: boolean;
  auto_summarize_threshold: number | null;
  show_artifacts: boolean;
}

// --- Providers (Phase 5) ---
export type NetworkTier = 'local' | 'personal' | 'remote';
export type ProviderType = 'ollama' | 'openrouter' | 'anthropic' | 'openai' | 'custom_openai';

export interface ProviderProfile {
  id: string;
  name: string;
  provider_type: ProviderType;
  base_url: string;
  models: string[];
  tools_enabled: boolean;
  is_trusted: boolean;
  temperature: number | null;
  top_p: number | null;
  context_length: number | null;
  /** STOPGAP client-side workaround (see chatStore.ts's send()) — Goose has no
      native hook for this yet; remove once block/goose#7617 or equivalent lands. */
  strip_reasoning: boolean;
  created_at: string;
}

/** ProviderProfile flattened + derived fields (mirrors Rust ProviderView). */
export interface ProviderView extends ProviderProfile {
  network_tier: NetworkTier;
  has_secret: boolean;
  active: boolean;
}

/** A file attached to a chat (Round-2 item 13): UTF-8 text, or a base64 data URL. */
export interface FileAttachment {
  name: string;
  kind: 'text' | 'binary';
  content: string;
  mime: string | null;
}

/** App-side chat-folder state (Round-2 item 15): folder list + session→folder map. */
export interface FolderData {
  folders: string[];
  assignments: Record<string, string>;
}

export interface OllamaModel {
  name: string;
  size: number;
  modified_at: string;
  details?: { parameter_size?: string; quantization_level?: string };
}

export interface PullProgress {
  pull_id: string;
  model: string;
  status: string;
  total?: number;
  completed?: number;
  done: boolean;
  error?: string;
}

export interface EnvVar {
  name: string;
  value: string | null;
}

export interface SettingsTarget {
  section: string;
  highlight: string | null;
}

// --- Wizard (Phase 7) ---
export interface DepStatus {
  installed: boolean;
  version: string | null;
  path: string | null;
  latest_version: string | null;
  is_outdated: boolean | null;
}

export interface Detection {
  ollama: DepStatus;
  goose: DepStatus;
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
  result: {
    stopReason?: string;
    /** Confirmed ACP `session/prompt` result shape (docs/acp-protocol.md). */
    usage?: { totalTokens?: number; inputTokens?: number; outputTokens?: number };
  };
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
