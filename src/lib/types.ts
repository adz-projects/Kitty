// Shared TS types. Mirror the Rust structs by hand; keep in sync.
// Pairs:
//   Config / NotificationPrefs  <-> src-tauri/src/config/mod.rs
//   StackStatus <-> src-tauri/src/state.rs; StackStatusPayload <-> src-tauri/src/lifecycle/health.rs
//   StartupPhase <-> src-tauri/src/state.rs; StartupPhasePayload <-> src-tauri/src/lifecycle/mod.rs

export interface NotificationPrefs {
  task_complete: boolean;
  approval_needed: boolean;
  task_failed: boolean;
  stack_degraded: boolean;
}

export interface Config {
  hotkeys: string[];
  clipboard_hotkey: string | null;
  open_window_hotkey: string | null;
  default_context_folder: string | null;
  ollama_base_url: string;
  setup_completed: boolean;
  theme: string;
  background_image: string | null;
  background_dim: number;
  background_position_x: number;
  background_position_y: number;
  background_size: 'cover' | 'contain' | 'stretch' | 'center';
  notifications: NotificationPrefs;
  remember_overlay_position: boolean;
  providers: ProviderProfile[];
  active_provider_id: string | null;
  strict_remote_mode: boolean;
  show_artifacts: boolean;
  /** Whether the in-process behavioral-memory (pathway) engine, linked
      directly into the BigTiny daemon, is active for this install. */
  adaptive_pathway_enabled: boolean;
  /** Ollama model tag used for the pathway engine's belief embeddings — one
      pinned tag shared by every user regardless of chat provider, so learned
      vectors live in the same space. */
  adaptive_pathway_embedding_model: string;
  /** Whether local inference (Ollama) is in play for this install — set by
      the wizard's first-screen fork, toggleable later from Advanced. */
  ollama_enabled: boolean;
  /** BigTiny background context-compaction settings — relayed to the daemon
      as `BIGTINY_SUMMARIZER__*` env vars at spawn time (Rust `Config::summarizer`,
      mirrors `bigtiny/config.py`'s `SummarizerConfig`). A daemon restart is
      needed for a change here to take effect. */
  summarizer: SummarizerSettings;
  /** BigTiny context-window/compaction budget settings — relayed as
      `BIGTINY_TOKEN_MANAGEMENT__*` env vars at spawn time (Rust
      `Config::token_management`, mirrors `bigtiny/config.py`'s
      `TokenManagementConfig`). A daemon restart is needed for a change
      here to take effect. */
  token_management: TokenManagementSettings;
  /** BigTiny pre-flight memory recall settings — relayed as
      `BIGTINY_MEMORY__*` env vars at spawn time (Rust `Config::memory`,
      mirrors `bigtiny_rust`'s `MemoryConfig`). A daemon restart is needed
      for a change here to take effect. */
  memory: MemorySettings;
}

export interface SummarizerSettings {
  enabled: boolean;
  model: string;
  keep_alive: string;
}

export interface TokenManagementSettings {
  max_context_tokens: number;
  max_live_tail_tokens: number;
  message_mask_head_lines: number;
  message_mask_tail_lines: number;
}

/** BigTiny pre-flight memory recall settings — relayed as
    `BIGTINY_MEMORY__*` env vars at spawn time (Rust `Config::memory`,
    mirrors `bigtiny_rust`'s `MemoryConfig`). A daemon restart is needed for
    a change here to take effect. */
export interface MemorySettings {
  /** Minimum FTS5 bm25 relevance score for pre-flight recall (higher =
      fewer, more relevant hits). `null` disables the gate. */
  bm25_threshold: number | null;
}

/** Daemon-global (all-session, process-lifetime) pre-flight memory recall
    telemetry — backs Settings > Advanced's "% of prompts with injected
    context" readout. */
export interface MemoryStats {
  total_prompts: number;
  injected_prompts: number;
  injection_rate_pct: number;
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
  is_trusted: boolean;
  temperature: number | null;
  top_p: number | null;
  /** llama.cpp/Ollama sampling extension, only ever sent to a self-hosted
      (ollama/custom_openai) endpoint — no effect on hosted OpenAI-compatible
      or Anthropic providers. */
  top_k: number | null;
  /** Same scoping as top_k. */
  min_p: number | null;
  /** Repetition control. Unlike temperature/top_p, `null` here does NOT mean
      "send nothing" — BigTiny fills in a repetition-safe default for
      self-hosted providers when this is unset, because llama-server's own
      default disables repetition control entirely (this is what let a
      quantized Qwen model stream an unbounded repetition loop). Set this to
      override that default, not to enable a penalty that doesn't otherwise
      exist. */
  presence_penalty: number | null;
  frequency_penalty: number | null;
  /** Hard cap on one reply's length. `null` gets BigTiny's own default for
      self-hosted providers (see presence_penalty). */
  max_tokens: number | null;
  context_length: number | null;
  /** STOPGAP client-side workaround (see chatStore.ts's send()) — Goose has no
      native hook for this yet; remove once block/goose#7617 or equivalent lands. */
  strip_reasoning: boolean;
  /** Custom system prompt; `null` = use the built-in mode-appropriate default
      (see system_prompts.ts). STOPGAP-adjacent — prepended client-side to a
      session's first message (chatStore.ts's send()), since Goose's ACP has no
      system-prompt param it honors. */
  system_prompt: string | null;
  /** Override for how long `session/prompt` tolerates silence before giving
      up, in seconds (default 300 when `null`). Raise this for a provider known
      to have long gaps between streamed updates (e.g. a slow Tailscale-hosted
      host); lower it if a long silence there reliably means "stuck" and
      waiting 5 minutes to find out is worse than a false-positive retry. */
  prompt_idle_timeout_secs: number | null;
  /** The `-np`/`--parallel` slot count this provider's own llama-server(-
      compatible) endpoint was started with, when known. `null` (the
      default) means never pin this provider's turns to a KV-cache slot —
      correct for Ollama and any endpoint not deliberately running a
      multi-slot llama-server. For prompt-cache determinism this must match
      the server's actual `--parallel` value exactly; a mismatch doesn't
      error, it just silently thrashes the KV cache instead of pinning it. */
  parallel_slots: number | null;
  created_at: string;
}

/** ProviderProfile flattened + derived fields (mirrors Rust ProviderView). */
export interface ProviderView extends ProviderProfile {
  network_tier: NetworkTier;
  has_secret: boolean;
  active: boolean;
}

/** `GET /api/v1/key`'s `data` object — only the fields Kitty's UI reads are
    named; the rest (byok_usage*, include_byok_in_limit, ...) pass through
    unread rather than being modeled in full. */
export interface OpenRouterCredits {
  label: string;
  limit: number | null;
  limit_remaining: number | null;
  usage: number;
  is_free_tier: boolean;
  [key: string]: unknown;
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

/** Mirrors `config::scheduled_tasks::Schedule` — a Rust internally-tagged
    enum, so the discriminant lives in `kind`. */
export type Schedule = { kind: 'one_shot' } | { kind: 'recurring'; interval_secs: number };

/** Mirrors `config::scheduled_tasks::ScheduledTask`. `next_fire` is an ISO
    8601 string (chrono's default `DateTime<Local>` serde representation). */
export interface ScheduledTask {
  id: string;
  name: string;
  prompt: string;
  cwd: string | null;
  schedule: Schedule;
  next_fire: string;
  enabled: boolean;
}

export interface OllamaModel {
  name: string;
  size: number;
  modified_at: string;
  details?: { parameter_size?: string; quantization_level?: string };
}

/** Mirrors `config::recipes::ParameterInputType`. */
export type ParameterInputType = 'string' | 'number' | 'boolean' | 'date' | 'file' | 'select';

/** Mirrors `config::recipes::ParameterRequirement`. `user_prompt` is the one
    parameter (at most one per recipe) whose value comes from whatever the
    user typed after `/slug` — see `src/lib/recipes.ts`'s `primaryParameter`. */
export type ParameterRequirement = 'required' | 'optional' | 'user_prompt';

/** Mirrors `config::recipes::RecipeParameter`. For the `user_prompt`
    parameter, `description` is user-facing invocation guidance shown in the
    slash-autocomplete dropdown and as the composer's placeholder — not just
    schema metadata. */
export interface RecipeParameter {
  key: string;
  input_type: ParameterInputType;
  requirement: ParameterRequirement;
  description: string;
  default: string | null;
  options: string[];
}

/** Known real-schema extension types. Only `builtin`/`platform`/`stdio` have
    an ACP equivalent Kitty can add to a live session (see
    `add_recipe_extension`); the rest are stored for YAML round-trip fidelity
    and silently skipped at launch. */
export type RecipeExtensionType =
  'stdio' | 'builtin' | 'platform' | 'streamable_http' | 'frontend' | 'inline_python';

/** Mirrors `config::recipes::RecipeExtension`. Extra real-schema fields
    Kitty doesn't specifically interpret pass through via an index signature
    (Rust's `#[serde(flatten)] extra: HashMap<...>`). */
export interface RecipeExtension {
  type: RecipeExtensionType;
  name: string;
  cmd?: string | null;
  args: string[];
  env_keys: string[];
  description?: string | null;
  timeout?: number | null;
  bundled?: boolean | null;
  [extra: string]: unknown;
}

/** Mirrors `config::recipes::Recipe`. A recipe is a client-side-interpreted
    template (Kitty attaches its `instructions`/`prompt`/`extensions` to an
    ordinary chat turn rather than shelling out to the real `goose run
    --recipe` CLI runner — see `docs/BACKLOG.md`'s now-resolved recipes entry
    and `chatStore.ts`'s `sendWithRecipe`). Still mirrors the real, portable
    Goose recipe YAML schema field-for-field so it round-trips through
    import/export as a real `.yaml` file — only `id`/`slug`/`is_builtin`/
    `created_at`/`max_reasoning_tokens` are Kitty-only, stripped on export. */
export interface Recipe {
  id: string;
  slug: string;
  title: string;
  description: string;
  instructions: string | null;
  prompt: string | null;
  version: string;
  parameters: RecipeParameter[];
  extensions: RecipeExtension[];
  activities: string[];
  is_builtin: boolean;
  created_at: string;
  /** Hard cap on how much a recipe-invoked turn is allowed to reason before
      Kitty auto-cancels it — see `chatStore.ts`'s `activeRecipeTurn`. Not
      part of the real Goose recipe schema (no ACP-exposed numeric reasoning-
      token config exists to query per model, only effort levels), so this is
      excluded from YAML export. Defaults to 2048. */
  max_reasoning_tokens: number;
}

/** Fields supplied when creating/updating a recipe — everything except the
    Kitty-only bookkeeping the backend owns. Mirrors Rust `RecipeInput`. */
export interface RecipeInput {
  slug: string;
  title: string;
  description: string;
  instructions: string | null;
  prompt: string | null;
  parameters: RecipeParameter[];
  extensions: RecipeExtension[];
  activities: string[];
  max_reasoning_tokens: number;
}

/** Mirrors Rust `RecipeImportResult` — a successfully-imported recipe plus
    any non-fatal warnings (e.g. a dropped `settings`/`response`/`retry`/
    `sub_recipes` block real Goose recipes can carry but Kitty can't apply). */
export interface RecipeImportResult {
  recipe: Recipe;
  warnings: string[];
}

/** Mirrors Rust `log_capture::LogEntry` — one captured WARN/ERROR tracing
    event, powering Settings → Advanced's error log. */
export interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
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

/** A BigTiny MCP server registration — daemon-global, live over REST (no
    restart to add/edit/delete/toggle). `streamable_http` is the MCP spec's
    successor to the old two-endpoint `sse` transport: a single POST endpoint,
    response framed as plain JSON or one SSE `data:` frame. `headers` carries
    auth for either remote transport (e.g. `{"Authorization": "Bearer ..."}`)
    — `stdio` never uses `url`/`headers`. */
export interface McpServer {
  id: string;
  name: string;
  transport: 'stdio' | 'sse' | 'streamable_http';
  command: string | null;
  args: string[];
  url: string | null;
  env: Record<string, string>;
  headers: Record<string, string>;
  enabled: boolean;
  status: 'connected' | 'disconnected' | 'error';
  error_message: string | null;
}

/** Input to `ipc.addMcpServer` — mirrors `McpServer` minus the server-assigned
    id/status fields. */
export interface McpServerSpec {
  name: string;
  transport: 'stdio' | 'sse' | 'streamable_http';
  command?: string | null;
  args?: string[];
  url?: string | null;
  env?: Record<string, string>;
  headers?: Record<string, string>;
  enabled?: boolean;
}

/** Input to `ipc.updateMcpServer` — every field optional, only what's set is
    changed. */
export interface McpServerPatch {
  name?: string;
  transport?: 'stdio' | 'sse' | 'streamable_http';
  command?: string | null;
  args?: string[];
  url?: string | null;
  env?: Record<string, string>;
  headers?: Record<string, string>;
  enabled?: boolean;
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
}

/** Result of `validate_setup` — powers the wizard's Done-step summary/soft
    Finish gate and Setup & Repair's lighter re-check. */
export interface SetupValidation {
  ready: boolean;
  issues: string[];
  adaptive_pathway_ok: boolean;
}

// Serde `rename_all = "snake_case"` on the Rust enum.
export type StackStatus =
  'starting' | 'ok' | 'ollama_down' | 'backend_down' | 'no_model' | 'provider_unreachable';

export interface StackStatusPayload {
  status: StackStatus;
  detail: string | null;
}

// One-time startup progress, separate from StackStatus (which is a
// steady-state health readout and has no concept of "spawning"/"warming").
// `ready` has no further bearing on chat availability once reached.
export type StartupPhase = 'spawning_backend' | 'warming_model' | 'ready';

export interface StartupPhasePayload {
  phase: StartupPhase;
}

// --- Behavioral-memory (pathway) engine — in-process inside BigTiny, see
// `plugins/adaptive-pathway_rust` and `plugins/bigtiny_rust/src/routes/pathway.rs`. ---

/** Readiness of the shared `qwen3-embedding:0.6b` Ollama model the pathway
    engine uses for belief embeddings — `downloading`/`missing` degrades
    gracefully to the engine's lexical-hashing fallback rather than reading
    as an outage. Mirrors `src-tauri/src/lifecycle/embedding.rs`. Queried
    only via the `adaptive_pathway://embedding_status` event (no on-demand
    getter command exists) — see `onAdaptivePathwayEmbeddingStatus`. */
export type EmbeddingModelStatus = 'unknown' | 'present' | 'downloading' | 'missing';

export interface AdaptivePathwayEmbeddingStatusPayload {
  status: EmbeddingModelStatus;
}

/** Connection state of the `pathway` in-process MCP server inside BigTiny.
    `status` is BigTiny's row field (`connected`/`error`/`disconnected`);
    `tool_count` is how many pathway tools (`record`/`forget`) are actually
    registered for the LLM tool list — 0 while connected-but-broken or
    unregistered. */
export interface AdaptivePathwayMcpStatus {
  status: string;
  error_message: string | null;
  tool_count: number;
}

/** A single behavioral-memory belief, as returned by `GET /api/pathway/beliefs`
    (`plugins/bigtiny_rust/src/routes/pathway.rs::list_beliefs`). */
export interface PathwayBelief {
  id: string;
  text: string;
  layer: 'identity' | 'context' | 'conversation';
  confidence: number;
  tested: boolean;
  domain: string | null;
  support_count: number;
  distinct_sessions: number;
  contradict_count: number;
  pinned: boolean;
}

/** `GET /api/pathway/stats` result — belief counts by layer plus
    embedding-model-migration progress (see
    `migrations/005_belief_embedding_model.sql` and
    `background::reembed_stale_beliefs` in `adaptive-pathway_rust`). */
export interface PathwayStats {
  total: number;
  by_layer: Record<string, number>;
  embedding_migration: {
    /** Beliefs still tagged with a stale embedding model, awaiting the
        background re-embed pass. */
    pending: number;
    current_model: string;
  };
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
  /** `null` when the active model doesn't support effort control at all (a
      single-option "off"-only model — see `parse_thinking_effort` in
      commands/session.rs). Live per-session control, no goosed restart. */
  thinking_effort: ThinkingEffort | null;
}

export interface EffortOption {
  name: string;
  value: string;
}

export interface ThinkingEffort {
  current_value: string;
  options: EffortOption[];
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
    /** Confirmed ACP `session/prompt` result shape (docs/acp-protocol.md).
        `cacheReadTokens`/`cacheCreationTokens` are absent entirely (not 0)
        for providers/models that don't report prompt-cache stats. */
    usage?: {
      totalTokens?: number;
      inputTokens?: number;
      outputTokens?: number;
      cacheReadTokens?: number;
      cacheCreationTokens?: number;
    };
    /** BigTiny's `llm_timing` SSE event, folded in by stream.rs — metrics for
        whichever LLM call in the turn produced the final visible text. */
    timing?: {
      ttfbMs?: number;
      ttftMs?: number;
      generationMs?: number;
      totalTokens?: number;
    };
  };
}

export interface ChatErrorEvent {
  session_id: string;
  message: string;
  error_type?: string; // "context_exceeded" | "insufficient_credits" | "other"
}

/** BigTiny's background context-compaction pass completed for this session
    (see `bigtiny/agent/compaction.py`). Delivered via a post-turn stats
    poll (`stream::poll_compaction_status`), not pushed live — the pass
    runs fire-and-forget after the turn's own SSE stream usually already
    closed, so it typically lands a few seconds after `chat://complete`. */
export interface CompactionEvent {
  session_id: string;
  compacted_through_rowid?: number;
  memory_slots?: {
    new_constraints?: string[];
    new_decisions?: string[];
    new_completions?: string[];
    current_state?: string;
  } | null;
  content?: string;
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

// --- Sessions / filesystem (Phase 4) ---

/** Mirrors src-tauri PathInfo. */
export interface PathInfo {
  path: string;
  name: string;
  is_dir: boolean;
  exists: boolean;
}

/** Mirrors `commands::file::FileEntry` — one file from a `list_directory`
    disk-scan (Artifacts pane, Round-7 item 5). */
export interface FileEntry {
  name: string;
  path: string;
  size: number;
  modified: number;
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
    title: String(raw.title ?? 'New Chat'),
    cwd: String(raw.cwd ?? ''),
    updatedAt: String(raw.updatedAt ?? ''),
    messageCount: typeof meta.messageCount === 'number' ? meta.messageCount : undefined,
    providerId: typeof meta.providerId === 'string' ? meta.providerId : undefined,
    modelId: typeof meta.modelId === 'string' ? meta.modelId : undefined,
  };
}
